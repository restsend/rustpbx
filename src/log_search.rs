//! Stateless, bounded historical reads of the configured log and its rotated files.

use aho_corasick::AhoCorasick;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const MAX_IDS: usize = 72;
const MAX_CURSOR_BYTES: usize = 65536;
const MAX_WINDOW_SECONDS: i64 = 86_400;
const MAX_FILES_PER_PAGE: usize = 3;
const MAX_CANDIDATES: usize = 32;
const MAX_SCAN_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 512 * 1024;
const MAX_LINES: usize = 200;
const READ_BYTES: usize = 16 * 1024;
const DEADLINE: Duration = Duration::from_secs(2);
static SEARCH_SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    pub node: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(deserialize_with = "deserialize_ids", serialize_with = "serialize_ids")]
    pub call_ids: Vec<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl SearchQuery {
    pub fn validate(&self) -> Result<(), String> {
        if self.end <= self.start
            || self.end - self.start > chrono::Duration::seconds(MAX_WINDOW_SECONDS)
        {
            return Err("Log search window must be positive and at most 24 hours".into());
        }
        if self.call_ids.is_empty() || self.call_ids.len() > MAX_IDS {
            return Err("Log search requires between 1 and 72 Call-IDs".into());
        }
        if self.call_ids.iter().any(|id| {
            id.is_empty()
                || id.len() > 256
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\'))
        }) {
            return Err("Complete, printable Call-IDs are required".into());
        }
        if self
            .limit
            .is_some_and(|value| value == 0 || value > MAX_LINES)
        {
            return Err("Log search limit must be between 1 and 200".into());
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|value| value.len() > MAX_CURSOR_BYTES)
        {
            return Err("Log search cursor is too large".into());
        }
        Ok(())
    }
}

fn deserialize_ids<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    let encoded = String::deserialize(deserializer)?;
    serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
}

fn serialize_ids<S: serde::Serializer>(ids: &[String], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&serde_json::to_string(ids).map_err(serde::ser::Error::custom)?)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    status: &'static str,
    available: bool,
    exists: bool,
    lines: Vec<String>,
    next_cursor: Option<String>,
    complete: bool,
    gaps: Vec<String>,
    scanned_bytes: usize,
    files: Vec<FileCoverage>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileCoverage {
    name: String,
    identity: String,
    captured_length: u64,
    from_position: u64,
    next_position: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct FileSnapshot {
    identity: String,
    len: u64,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    node: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    call_ids: Vec<String>,
    files: Vec<FileSnapshot>,
    file: usize,
    offset: u64,
    timestamp: Option<i64>,
    skipping: bool,
}

struct Candidate {
    path: PathBuf,
    snapshot: FileSnapshot,
    priority: u8,
}

struct Budget {
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    io: Arc<AtomicUsize>,
}

impl Budget {
    fn stopped(&self) -> bool {
        self.io.load(Ordering::Relaxed) >= MAX_SCAN_BYTES
            || Instant::now() >= self.deadline
            || self.cancel.load(Ordering::Relaxed)
    }
}

struct LimitedFile {
    file: File,
    io: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}

impl Read for LimitedFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
            return Ok(0);
        }
        let count = buffer
            .len()
            .min(MAX_SCAN_BYTES.saturating_sub(self.io.load(Ordering::Relaxed)));
        let read = self.file.read(&mut buffer[..count])?;
        self.io.fetch_add(read, Ordering::Relaxed);
        Ok(read)
    }
}

impl Seek for LimitedFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

#[derive(Debug)]
struct SearchError(StatusCode, &'static str);

impl From<std::io::Error> for SearchError {
    fn from(error: std::io::Error) -> Self {
        tracing::warn!(%error, "Historical log search I/O failed");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "Log search I/O failed")
    }
}

fn changed() -> SearchError {
    SearchError(
        StatusCode::CONFLICT,
        "Log search evidence changed; start a new query",
    )
}

fn identity(metadata: &fs::Metadata) -> String {
    format!("{}-{}", metadata.dev(), metadata.ino())
}

fn open_regular(path: &Path) -> Result<File, SearchError> {
    #[cfg(target_os = "linux")]
    const NO_FOLLOW: i32 = 0x20000;
    #[cfg(target_os = "macos")]
    const NO_FOLLOW: i32 = 0x100;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const NO_FOLLOW: i32 = 0;
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(SearchError(
            StatusCode::BAD_REQUEST,
            "Log search requires regular files",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NO_FOLLOW)
        .open(path)?;
    let after = file.metadata()?;
    if !after.is_file() || identity(&before) != identity(&after) {
        return Err(changed());
    }
    Ok(file)
}

fn archive_period(suffix: &str) -> Option<(DateTime<Utc>, chrono::Duration)> {
    use chrono::TimeZone;
    let mut parts = suffix.split('.');
    let period = parts.next()?;
    if let Some(collision) = parts.next()
        && (collision.is_empty() || !collision.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    let (start, hours) = match period.len() {
        13 => (
            chrono::NaiveDateTime::parse_from_str(&format!("{period}:00:00"), "%Y-%m-%d-%H:%M:%S")
                .ok()?,
            1,
        ),
        10 => (
            chrono::NaiveDate::parse_from_str(period, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)?,
            24,
        ),
        _ => return None,
    };
    Some((
        chrono::Local
            .from_local_datetime(&start)
            .earliest()?
            .with_timezone(&Utc),
        chrono::Duration::hours(hours),
    ))
}

fn candidate(path: PathBuf, priority: u8) -> Result<Option<Candidate>, SearchError> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SearchError(
            StatusCode::BAD_REQUEST,
            "Log search refuses symbolic links",
        ));
    }
    Ok(Some(Candidate {
        path,
        snapshot: FileSnapshot {
            identity: identity(&metadata),
            len: metadata.len(),
        },
        priority,
    }))
}

fn candidates(
    path: &Path,
    query: &SearchQuery,
    budget: &Budget,
) -> Result<(Vec<Candidate>, Vec<String>), SearchError> {
    if budget.stopped() {
        return Err(SearchError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Log search was cancelled",
        ));
    }
    // A bare filename has an empty parent; enumerate the current directory instead.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SearchError(
            StatusCode::BAD_REQUEST,
            "Invalid configured log path",
        ))?;
    let mut files = Vec::new();
    let mut gaps = Vec::new();
    let base = parent.join(name);
    if let Some(file) = candidate(base.clone(), 1)? {
        files.push(file);
    }
    // Known period paths are probed before bounded directory enumeration can omit them.
    for hour in 0..=25 {
        if budget.stopped() {
            gaps.push("directory_enumeration_incomplete".into());
            break;
        }
        let Some(time) = query
            .start
            .checked_add_signed(chrono::Duration::hours(hour))
        else {
            break;
        };
        if time - query.end > chrono::Duration::hours(1) {
            break;
        }
        let local = time.with_timezone(&chrono::Local);
        for suffix in [
            local.format("%Y-%m-%d-%H").to_string(),
            local.format("%Y-%m-%d").to_string(),
        ] {
            let Some((start, period)) = archive_period(&suffix) else {
                continue;
            };
            if start > query.end || start + period < query.start {
                continue;
            }
            let path = parent.join(format!("{name}.{suffix}"));
            if !files.iter().any(|file: &Candidate| file.path == path)
                && let Some(file) = candidate(path, 0)?
            {
                files.push(file);
            }
        }
    }
    for entry in fs::read_dir(parent)? {
        if budget.stopped() {
            gaps.push("directory_enumeration_incomplete".into());
            break;
        }
        let entry = entry?;
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        let priority = if filename == name {
            1
        } else if let Some((start, period)) = filename
            .strip_prefix(&format!("{name}."))
            .and_then(archive_period)
        {
            // With normal clock ordering, enqueue delay only moves events to later write periods.
            if start + period < query.start {
                continue;
            }
            if start <= query.end { 0 } else { 2 }
        } else {
            continue;
        };
        let path = entry.path();
        if !files.iter().any(|file| file.path == path)
            && let Some(file) = candidate(path, priority)?
        {
            files.push(file);
        }
    }
    files.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.path.cmp(&right.path))
    });
    if files.len() > MAX_CANDIDATES {
        // The active file can contain delayed events even when every archive slot is occupied.
        if let Some(index) = files.iter().position(|file| file.path == base)
            && index >= MAX_CANDIDATES
        {
            files.swap(MAX_CANDIDATES - 1, index);
        }
        files.truncate(MAX_CANDIDATES);
        gaps.push("candidate_file_limit_exceeded".into());
    }
    Ok((files, gaps))
}

fn matches_id(line: &[u8], matcher: &AhoCorasick) -> bool {
    fn id_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_'
                    | b'.'
                    | b'@'
                    | b'+'
                    | b'%'
                    | b'!'
                    | b'*'
                    | b'`'
                    | b'\''
                    | b'~'
                    | b'('
                    | b')'
                    | b'<'
                    | b'>'
                    | b':'
                    | b'/'
                    | b'['
                    | b']'
                    | b'?'
                    | b'{'
                    | b'}'
            )
    }
    // Overlapping patterns preserve the longer complete ID when a shorter prefix fails its boundary.
    matcher.find_overlapping_iter(line).any(|found| {
        (found.start() == 0 || !id_byte(line[found.start() - 1]))
            && (found.end() == line.len() || !id_byte(line[found.end()]))
    })
}

fn read_line(
    reader: &mut BufReader<LimitedFile>,
    end: u64,
    budget: &Budget,
    skipping: &mut bool,
) -> Result<Option<(Vec<u8>, bool)>, SearchError> {
    let mut line = Vec::new();
    loop {
        if budget.stopped() {
            return Ok(None);
        }
        let position = reader.stream_position()?;
        if position >= end {
            let omitted = std::mem::take(skipping);
            return Ok(if line.is_empty() && !omitted {
                None
            } else {
                Some((line, omitted))
            });
        }
        let data = reader.fill_buf()?;
        if data.is_empty() {
            return if budget.stopped() {
                Ok(None)
            } else {
                Err(changed())
            };
        }
        let length = data.len().min((end - position) as usize);
        let count = data[..length]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(length, |offset| offset + 1);
        let finished = data[count - 1] == b'\n';
        if !*skipping && line.len() + count <= MAX_LINE_BYTES {
            line.extend_from_slice(&data[..count]);
        } else {
            *skipping = true;
            line.clear();
        }
        reader.consume(count);
        if finished {
            let omitted = std::mem::take(skipping);
            return Ok(Some((line, omitted)));
        }
    }
}

fn search(
    path: &Path,
    query: SearchQuery,
    cancel: Arc<AtomicBool>,
) -> Result<SearchResult, SearchError> {
    let budget = Budget {
        deadline: Instant::now() + DEADLINE,
        cancel,
        io: Arc::new(AtomicUsize::new(0)),
    };
    let matcher = AhoCorasick::new(&query.call_ids).map_err(|_| {
        SearchError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Log search matcher initialization failed",
        )
    })?;
    let (files, gaps) = candidates(path, &query, &budget)?;
    let mut cursor = if let Some(encoded) = &query.cursor {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| SearchError(StatusCode::BAD_REQUEST, "Invalid log cursor"))?;
        let cursor: Cursor = serde_json::from_slice(&bytes)
            .map_err(|_| SearchError(StatusCode::BAD_REQUEST, "Invalid log cursor"))?;
        if cursor.node != crate::utils::self_hostname()
            || cursor.start != query.start
            || cursor.end != query.end
            || cursor.call_ids != query.call_ids
        {
            return Err(changed());
        }
        cursor
    } else {
        Cursor {
            node: crate::utils::self_hostname().to_string(),
            start: query.start,
            end: query.end,
            call_ids: query.call_ids.clone(),
            files: files.iter().map(|file| file.snapshot.clone()).collect(),
            file: 0,
            offset: 0,
            timestamp: None,
            skipping: false,
        }
    };
    // Pin the initial file collection and byte lengths. Append is visible on an explicit new query;
    // rotation that introduces/removes an inode invalidates continuation instead of hiding events.
    if cursor.files.len() != files.len()
        || cursor.file > cursor.files.len()
        || cursor.files.iter().any(|snapshot| {
            !files.iter().any(|file| {
                file.snapshot.identity == snapshot.identity && file.snapshot.len >= snapshot.len
            })
        })
    {
        return Err(changed());
    }
    let mut result = SearchResult {
        status: "ok",
        available: true,
        exists: !files.is_empty(),
        lines: Vec::new(),
        next_cursor: None,
        complete: false,
        gaps,
        scanned_bytes: 0,
        files: Vec::new(),
    };
    let mut returned_bytes = 0;
    let mut page_full = false;
    while cursor.file < cursor.files.len()
        && result.files.len() < MAX_FILES_PER_PAGE
        && !budget.stopped()
    {
        let snapshot = &cursor.files[cursor.file];
        let file = files
            .iter()
            .find(|file| file.snapshot.identity == snapshot.identity)
            .ok_or_else(changed)?;
        if cursor.offset > snapshot.len {
            return Err(changed());
        }
        let handle = open_regular(&file.path)?;
        let metadata = handle.metadata()?;
        if identity(&metadata) != snapshot.identity || metadata.len() < snapshot.len {
            return Err(changed());
        }
        let mut reader = BufReader::with_capacity(
            READ_BYTES,
            LimitedFile {
                file: handle,
                io: budget.io.clone(),
                cancel: budget.cancel.clone(),
                deadline: budget.deadline,
            },
        );
        if cursor.offset > 0 && cursor.offset < snapshot.len && !cursor.skipping {
            reader.seek(SeekFrom::Start(cursor.offset - 1))?;
            let mut previous = [0];
            reader.read_exact(&mut previous)?;
            if previous[0] != b'\n' {
                return Err(changed());
            }
        }
        reader.seek(SeekFrom::Start(cursor.offset))?;
        let from = cursor.offset;
        while cursor.offset < snapshot.len && !budget.stopped() {
            let before = cursor.offset;
            let Some((bytes, omitted)) =
                read_line(&mut reader, snapshot.len, &budget, &mut cursor.skipping)?
            else {
                cursor.offset = if cursor.skipping {
                    reader.stream_position()?
                } else {
                    before
                };
                break;
            };
            cursor.offset = reader.stream_position()?;
            if omitted {
                result.gaps.push("line_byte_limit_exceeded".into());
                continue;
            }
            let line = String::from_utf8_lossy(&bytes);
            if let Some(token) = line.split_whitespace().next()
                && let Ok(time) = DateTime::parse_from_rfc3339(token)
            {
                cursor.timestamp = Some(time.timestamp_micros());
            }
            if !bytes.ends_with(b"\n") {
                result.gaps.push("incomplete_line".into());
            }
            if let Some(time) = cursor.timestamp {
                if time >= query.start.timestamp_micros()
                    && time <= query.end.timestamp_micros()
                    && matches_id(&bytes, &matcher)
                {
                    if returned_bytes + line.len() > MAX_RESULT_BYTES {
                        cursor.offset = before;
                        page_full = true;
                        break;
                    }
                    returned_bytes += line.len();
                    result
                        .lines
                        .push(line.trim_end_matches(['\r', '\n']).to_owned());
                }
            } else {
                result.gaps.push("timestamp_unavailable".into());
            }
            if result.lines.len() >= query.limit.unwrap_or(MAX_LINES) {
                break;
            }
        }
        result.files.push(FileCoverage {
            name: file
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            identity: snapshot.identity.clone(),
            captured_length: snapshot.len,
            from_position: from,
            next_position: cursor.offset,
        });
        if cursor.offset >= snapshot.len {
            cursor.file += 1;
            cursor.offset = 0;
            cursor.timestamp = None;
            cursor.skipping = false;
        }
        if page_full || result.lines.len() >= query.limit.unwrap_or(MAX_LINES) {
            break;
        }
    }
    result.scanned_bytes = budget.io.load(Ordering::Relaxed);
    result.gaps.sort();
    result.gaps.dedup();
    let finished = cursor.file == cursor.files.len();
    result.complete = finished && result.gaps.is_empty();
    if !finished {
        result.next_cursor = Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).map_err(
            |_| {
                SearchError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Log cursor encoding failed",
                )
            },
        )?));
    }
    if !result.complete {
        result.status = "partial";
    }
    tracing::debug!(
        scanned_bytes = result.scanned_bytes,
        files = result.files.len(),
        complete = result.complete,
        "Historical log search completed"
    );
    Ok(result)
}

pub async fn search_response(path: Option<String>, query: SearchQuery) -> Response {
    if let Err(message) = query.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status":"error","message":message})),
        )
            .into_response();
    }
    let Some(path) = path else {
        return Json(serde_json::json!({"status":"partial","available":false,"exists":false,"lines":[],
            "complete":false,"nextCursor":null,"gaps":["log_file_not_configured"],"scannedBytes":0,"files":[]})).into_response();
    };
    let permit = match SEARCH_SLOT
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
    {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"status":"error","message":"Log search is busy"})),
            )
                .into_response();
        }
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        search(Path::new(&path), query, worker_cancel)
    });
    let outcome = tokio::select! {
        result = &mut task => result,
        _ = tokio::time::sleep(DEADLINE) => { cancel.store(true, Ordering::Relaxed); task.await }
    };
    match outcome {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(SearchError(status, message))) => (
            status,
            Json(serde_json::json!({"status":"error","message":message})),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "Historical log search worker failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status":"error","message":"Log search worker failed"})),
            )
                .into_response()
        }
    }
}
