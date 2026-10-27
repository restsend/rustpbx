//! Shared log-viewer helpers.
//!
//! Single implementation of "read the tail / follow window of the configured
//! log file" used by both the console settings endpoints
//! (`/settings/logs/...`, superuser-facing) and the AMI cluster log endpoints
//! (`/cluster/logs/...`, peer-facing so other nodes can proxy a node's logs
//! for the console cluster log viewer).

pub use crate::log_search::{SearchQuery, search_response};

use serde_json::{Value as JsonValue, json};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const LOG_DEFAULT_LIMIT: usize = 200;
pub const LOG_MAX_LIMIT: usize = 5000;
const LOG_READ_BYTES: usize = 4 * 1024 * 1024;
const LOG_LINE_BYTES: usize = 64 * 1024;
const LOG_RESULT_BYTES: usize = 512 * 1024;
const LOG_BLOCK_BYTES: usize = 16 * 1024;
const LOG_PENDING_FILES: usize = 1024;
const LOG_DIRECTORY_TIMEOUT: Duration = Duration::from_secs(2);

pub struct FollowReadResult {
    pub lines: Vec<String>,
    pub next_position: u64,
    pub reset: bool,
    pub truncated: bool,
    pub gaps: Vec<&'static str>,
    pub file_identity: String,
    pub read_file_identity: String,
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

/// Project active, non-sensitive logging settings from the node owning the log cursor.
pub fn logging_metadata_from_config(config: &crate::config::Config) -> JsonValue {
    let media = config.rtp_config();
    json!({
        "revision": env!("GIT_COMMIT_HASH"),
        "log_level": config.log_level,
        "log_rotation": config.log_rotation,
        "sipflow_configured": config.sipflow.is_some(),
        "quality_stats": media.quality_stats,
        "volume_stats": media.volume_stats,
    })
}

pub fn read_recent_log_lines(path: &str, limit: usize) -> io::Result<FollowReadResult> {
    read_log_tail(File::open(path)?, limit)
}

fn read_log_tail(mut file: File, limit: usize) -> io::Result<FollowReadResult> {
    let metadata = file.metadata()?;
    let end = metadata.len();
    let file_identity = log_file_identity(&metadata);
    let mut position = end;
    let mut chunks = Vec::new();
    let mut bytes = 0;
    let mut newlines = 0;
    while position > 0 && bytes < LOG_READ_BYTES && newlines <= limit {
        let count = LOG_BLOCK_BYTES
            .min(LOG_READ_BYTES - bytes)
            .min(position as usize);
        position -= count as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0; count];
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        bytes += count;
        chunks.push(chunk);
    }
    let data: Vec<u8> = chunks.into_iter().rev().flatten().collect();
    let data = data.strip_suffix(b"\n").unwrap_or(&data);
    let mut lines = Vec::new();
    let mut gaps = Vec::new();
    let mut result_bytes = 0;
    let mut truncated = position > 0;
    let mut pieces = data.rsplit(|byte| *byte == b'\n').peekable();
    while let Some(line) = pieces.next() {
        if line.is_empty() && data.is_empty() {
            break;
        }
        // The leading fragment of a bounded tail is not a complete log event.
        if position > 0 && pieces.peek().is_none() {
            if newlines <= limit {
                gaps.push("read_limit");
            }
            break;
        }
        if lines.len() == limit {
            truncated = true;
            break;
        }
        if line.len() > LOG_LINE_BYTES {
            if !gaps.contains(&"line_too_long") {
                gaps.push("line_too_long");
            }
            truncated = true;
            continue;
        }
        if result_bytes + line.len() > LOG_RESULT_BYTES {
            gaps.push("result_limit");
            truncated = true;
            break;
        }
        let line = std::str::from_utf8(line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        result_bytes += line.len();
        lines.push(line.trim_end_matches('\r').to_string());
    }
    lines.reverse();
    Ok(FollowReadResult {
        lines,
        next_position: end,
        reset: false,
        truncated,
        gaps,
        read_file_identity: file_identity.clone(),
        file_identity,
    })
}

fn log_file_identity(metadata: &fs::Metadata) -> String {
    format!("{}-{}", metadata.dev(), metadata.ino())
}

pub fn valid_file_identity(value: &str) -> bool {
    value.len() <= 41
        && value.split_once('-').is_some_and(|(device, inode)| {
            !device.is_empty()
                && !inode.is_empty()
                && device.bytes().all(|byte| byte.is_ascii_digit())
                && inode.bytes().all(|byte| byte.is_ascii_digit())
                && device.parse::<u64>().is_ok()
                && inode.parse::<u64>().is_ok()
        })
}

fn archive_order(name: &str, base: &str) -> Option<(String, u64)> {
    let suffix = name.strip_prefix(base)?.strip_prefix('.')?;
    let (period, collision) = match suffix.split_once('.') {
        Some((period, collision)) => {
            if collision.is_empty() || !collision.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let collision = collision.parse::<u64>().ok()?;
            if collision == 0 {
                return None;
            }
            (period, collision)
        }
        None => (suffix, 0),
    };
    if !matches!(period.len(), 10 | 13) || !period.is_ascii() {
        return None;
    }
    chrono::NaiveDate::parse_from_str(&period[..10], "%Y-%m-%d").ok()?;
    if period.len() == 13
        && (period.as_bytes()[10] != b'-'
            || !period[11..].bytes().all(|byte| byte.is_ascii_digit())
            || period[11..].parse::<u8>().ok()? > 23)
    {
        return None;
    }
    Some((period.to_string(), collision))
}

fn archived_log_files(path: &Path, identity: &str) -> io::Result<Option<Vec<PathBuf>>> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid log file name"))?;
    let inode = identity
        .split_once('-')
        .and_then(|(_, inode)| inode.parse::<u64>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid log file identity"))?;
    let deadline = Instant::now() + LOG_DIRECTORY_TIMEOUT;
    let mut current = None;
    // Directory inodes locate the retained cursor without opening every historical archive.
    for entry in fs::read_dir(directory)? {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let entry = entry?;
        if entry.ino() != inode {
            continue;
        }
        let order = entry
            .file_name()
            .to_str()
            .and_then(|name| archive_order(name, base));
        if order.is_some()
            && entry.file_type()?.is_file()
            && log_file_identity(&entry.metadata()?) == identity
        {
            current = order;
            break;
        }
    }
    let Some(current) = current else {
        return Ok(Some(Vec::new()));
    };
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let entry = entry?;
        let Some(order) = entry
            .file_name()
            .to_str()
            .and_then(|name| archive_order(name, base))
        else {
            continue;
        };
        // Older periods precede the cursor and cannot consume the pending-segment budget.
        if order < current {
            continue;
        }
        if entry.file_type()?.is_file() {
            if files.len() == LOG_PENDING_FILES {
                return Ok(None);
            }
            files.push((order, entry.path()));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(Some(files.into_iter().map(|(_, path)| path).collect()))
}

pub fn read_follow_log_lines(
    path: &str,
    position: u64,
    limit: usize,
    file_identity: Option<&str>,
) -> io::Result<FollowReadResult> {
    let mut base = match File::open(path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let base_identity = base
        .as_ref()
        .map(File::metadata)
        .transpose()?
        .as_ref()
        .map(log_file_identity);
    if file_identity.is_none() || file_identity == base_identity.as_deref() {
        return read_follow_file(
            base.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Active log file is unavailable")
            })?,
            position,
            limit,
        );
    }
    let identity = file_identity.unwrap_or_default();
    if !valid_file_identity(identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid log file identity",
        ));
    }
    let Some(files) = archived_log_files(Path::new(path), identity)? else {
        let mut result = read_follow_file(
            base.take().ok_or_else(|| {
                io::Error::other("Active log file is unavailable during recovery")
            })?,
            0,
            limit,
        )?;
        result.reset = true;
        result.gaps.push("directory_limit");
        result.truncated = true;
        return Ok(result);
    };
    for (index, path) in files.iter().enumerate() {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if log_file_identity(&file.metadata()?) != identity {
            continue;
        }
        let end = file.metadata()?.len();
        let mut result = read_follow_file(file, position, limit)?;
        if result.next_position == end {
            // The next cursor belongs to the next segment; rows retain their original file identity.
            let next_identity = match files.get(index + 1) {
                Some(next) => {
                    let file = File::open(next).map_err(|error| {
                        io::Error::other(format!("Next log segment is unavailable: {error}"))
                    })?;
                    Some(log_file_identity(&file.metadata()?))
                }
                None => base_identity,
            };
            // A writer may be between renaming the old file and creating the new active file.
            if let Some(next_identity) = next_identity {
                result.file_identity = next_identity;
                result.next_position = 0;
                result.truncated = true;
            }
        }
        return Ok(result);
    }
    let mut result = read_follow_file(
        base.take()
            .ok_or_else(|| io::Error::other("Active log file is unavailable during recovery"))?,
        0,
        limit,
    )?;
    result.reset = true;
    result.gaps.push("file_unavailable");
    result.truncated = true;
    Ok(result)
}

fn read_follow_file(
    mut file: File,
    mut position: u64,
    limit: usize,
) -> io::Result<FollowReadResult> {
    let metadata = file.metadata()?;
    let end = metadata.len();
    let file_identity = log_file_identity(&metadata);
    let reset = position > end;
    if reset {
        position = 0;
    }
    file.seek(SeekFrom::Start(position))?;
    let mut reader = BufReader::with_capacity(
        LOG_BLOCK_BYTES,
        file.take((end - position).min(LOG_READ_BYTES as u64)),
    );
    let mut lines = Vec::new();
    let mut gaps = if reset {
        vec!["file_truncated"]
    } else {
        Vec::new()
    };
    let mut next_position = position;
    let mut read_bytes = 0;
    let mut result_bytes = 0;
    while lines.len() < limit && next_position < end && read_bytes < LOG_READ_BYTES {
        let start = next_position;
        let mut line = Vec::new();
        let mut overlong = false;
        let mut finished = false;
        while read_bytes < LOG_READ_BYTES {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Log file changed during read",
                ));
            }
            let length = buffer.len().min(LOG_READ_BYTES - read_bytes);
            let count = buffer[..length]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(length, |offset| offset + 1);
            finished = buffer[count - 1] == b'\n';
            if !overlong && line.len() + count <= LOG_LINE_BYTES {
                line.extend_from_slice(&buffer[..count]);
            } else {
                overlong = true;
                line.clear();
            }
            reader.consume(count);
            read_bytes += count;
            next_position += count as u64;
            if finished || next_position == end {
                finished = true;
                break;
            }
        }
        if !finished {
            // Keep a partial event at its original boundary; consumers must retain this gap.
            next_position = start;
            gaps.push("read_limit");
            break;
        }
        if overlong {
            if !gaps.contains(&"line_too_long") {
                gaps.push("line_too_long");
            }
            continue;
        }
        if result_bytes + line.len() > LOG_RESULT_BYTES {
            next_position = start;
            break;
        }
        let line = std::str::from_utf8(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        result_bytes += line.len();
        lines.push(line.trim_end_matches(&['\n', '\r'][..]).to_string());
    }
    let truncated = next_position < end || !gaps.is_empty();
    Ok(FollowReadResult {
        lines,
        next_position,
        reset,
        truncated,
        gaps,
        read_file_identity: file_identity.clone(),
        file_identity,
    })
}

async fn log_payload(
    path: Option<&str>,
    position: Option<u64>,
    limit: usize,
    file_identity: Option<&str>,
) -> Result<JsonValue, String> {
    let path = path.map(str::to_owned);
    let file_identity = file_identity.map(str::to_owned);
    tokio::task::spawn_blocking(move || match position {
        Some(position) => {
            follow_log_payload_blocking(path.as_deref(), position, limit, file_identity.as_deref())
        }
        None => recent_log_payload_blocking(path.as_deref(), limit),
    })
    .await
    .map_err(|error| format!("Log reader worker failed: {error}"))?
}

pub async fn recent_log_payload(path: Option<&str>, limit: usize) -> Result<JsonValue, String> {
    log_payload(path, None, limit, None).await
}

pub async fn follow_log_payload(
    path: Option<&str>,
    position: u64,
    limit: usize,
    file_identity: Option<&str>,
) -> Result<JsonValue, String> {
    log_payload(path, Some(position), limit, file_identity).await
}

/// Build the JSON payload for a "recent logs" request. `Ok(payload)` covers
/// the ok / not-configured / not-found cases; `Err(message)` reports hard
/// I/O failures so callers can map them to their own error responses.
fn recent_log_payload_blocking(path: Option<&str>, limit: usize) -> Result<JsonValue, String> {
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
            "gaps": [],
            "message": "Log file is not configured. Set settings -> platform -> log_file first.",
        }));
    };

    match read_recent_log_lines(path, limit) {
        Ok(result) => Ok(json!({
            "status": "ok",
            "available": true,
            "exists": true,
            "path": path,
            "lines": result.lines,
            "next_position": result.next_position,
            "reset": false,
            "truncated": result.truncated,
            "gaps": result.gaps,
            "file_identity": result.file_identity,
            "read_file_identity": result.read_file_identity,
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
            "gaps": [],
            "message": "Log file does not exist yet.",
        })),
        Err(err) => Err(format!("Failed to read log file: {err}")),
    }
}

/// Build the JSON payload for a polling "follow logs" request. Same
/// contract as [`recent_log_payload`].
fn follow_log_payload_blocking(
    path: Option<&str>,
    position: u64,
    limit: usize,
    file_identity: Option<&str>,
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
            "gaps": [],
            "message": "Log file is not configured. Set settings -> platform -> log_file first.",
        }));
    };

    match read_follow_log_lines(path, position, limit, file_identity) {
        Ok(result) => Ok(json!({
            "status": "ok",
            "available": true,
            "exists": true,
            "path": path,
            "lines": result.lines,
            "next_position": result.next_position,
            "reset": result.reset,
            "truncated": result.truncated,
            "gaps": result.gaps,
            "file_identity": result.file_identity,
            "read_file_identity": result.read_file_identity,
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
            "gaps": ["file_unavailable"],
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
pub async fn follow_log_stream_frame(
    path: &str,
    position: u64,
    limit: usize,
    file_identity: Option<&str>,
) -> JsonValue {
    match follow_log_payload(Some(path), position, limit, file_identity).await {
        Ok(mut payload) => {
            if payload["exists"] == false {
                payload["reset"] = json!(true);
            }
            payload
        }
        Err(message) => json!({ "status": "error", "message": message }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn read_recent_log_lines_limits_tail() {
        {
            let mut file = NamedTempFile::new().expect("tempfile");
            file.write_all(&[0xff]).expect("write historical prefix");
            file.as_file_mut()
                .set_len(8 * 1024 * 1024)
                .expect("large historical prefix");
            file.seek(SeekFrom::End(0)).expect("seek tail");
            writeln!(file, "\nline-1\nline-2\nline-3").expect("write tail");
            let end = file.as_file().metadata().expect("metadata").len();
            let path = file.path().to_string_lossy().to_string();
            let payload = recent_log_payload(Some(&path), 2)
                .await
                .expect("read only recent tail");
            assert_eq!(payload["lines"], json!(["line-2", "line-3"]));
            assert_eq!(payload["next_position"], end);
            assert_eq!(payload["reset"], false);
            assert_eq!(payload["truncated"], true);
            assert_eq!(payload["gaps"], json!([]));
        }
    }

    #[tokio::test]
    async fn follow_logs_keeps_position_when_truncated() {
        for length in [128 * 1024, 8 * 1024 * 1024, 0] {
            let mut file = NamedTempFile::new().expect("tempfile");
            writeln!(file, "l1\nl2").expect("write first page");
            let page_end = file.stream_position().expect("position");
            if length > 0 {
                file.write_all(&vec![b'x'; length])
                    .expect("write overlong line");
                writeln!(file).expect("finish overlong line");
            }
            writeln!(file, "l3").expect("write last line");
            let end = file.stream_position().expect("end");
            let path = file.path().to_string_lossy().to_string();
            let first = follow_log_payload(Some(&path), 0, 2, None)
                .await
                .expect("first follow");
            assert_eq!(first["lines"], json!(["l1", "l2"]));
            assert_eq!(first["next_position"], page_end);
            assert_eq!(first["truncated"], true);
            let second = follow_log_payload(Some(&path), page_end, 2, None)
                .await
                .expect("second follow");
            let blocked = length > 4 * 1024 * 1024;
            assert_eq!(
                second["lines"],
                if blocked { json!([]) } else { json!(["l3"]) }
            );
            assert_eq!(
                second["next_position"],
                if blocked { page_end } else { end }
            );
            assert_eq!(second["reset"], false);
            assert_eq!(first["gaps"], json!([]));
            assert_eq!(second["truncated"], length > 0);
            assert_eq!(
                second["gaps"],
                if blocked {
                    json!(["read_limit"])
                } else if length > 0 {
                    json!(["line_too_long"])
                } else {
                    json!([])
                }
            );
        }

        let mut file = NamedTempFile::new().expect("tempfile");
        for _ in 0..9 {
            writeln!(file, "{}", "x".repeat(60 * 1024)).expect("write bounded long lines");
        }
        let path = file.path().to_string_lossy().to_string();
        let first = follow_log_payload(Some(&path), 0, 20, None)
            .await
            .expect("bounded result");
        assert_eq!(first["lines"].as_array().expect("lines").len(), 8);
        assert_eq!(first["truncated"], true);
        assert_eq!(first["gaps"], json!([]));
        let next = first["next_position"].as_u64().expect("cursor");
        let second = follow_log_payload(Some(&path), next, 20, None)
            .await
            .expect("resume bounded result");
        assert_eq!(second["lines"].as_array().expect("lines").len(), 1);
        assert_eq!(second["truncated"], false);
        assert_eq!(second["gaps"], json!([]));
    }

    #[tokio::test]
    async fn recent_log_payload_reports_unconfigured() {
        let payload = recent_log_payload(None, 200).await.expect("payload");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["available"], false);
        assert_eq!(payload["exists"], false);
        assert!(payload["lines"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn follow_log_payload_reports_missing_file() {
        let payload = follow_log_payload(Some("/nonexistent/rustpbx-log-test.log"), 5, 100, None)
            .await
            .expect("payload");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["exists"], false);
        assert_eq!(payload["reset"], true);
    }

    #[tokio::test]
    async fn follow_log_stream_frame_error_has_no_next_position() {
        let frame =
            follow_log_stream_frame("/nonexistent/rustpbx-log-test.log", 0, 100, None).await;
        // not-found frames reset the cursor to 0
        assert_eq!(frame["next_position"], 0u64);
        assert_eq!(frame["reset"], true);
    }
}
