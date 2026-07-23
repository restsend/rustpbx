use crate::config::SipFlowSubdirs;
use crate::sipflow::flusher::{FlushCommand, FlushMeta};
use crate::sipflow::protocol::{MsgType, Packet};
use crate::sipflow::rtp_stats::{MediaStatsAccumulator, parse_rtp_stats_header};
use crate::sipflow::{SipFlowItem, SipFlowMediaStats, SipFlowMsgType};
use anyhow::Result;
use bytes::Bytes;
use chrono::{DateTime, Datelike, Local, Timelike};
use futures::TryStreamExt;
use sqlx::{Connection, SqliteConnection};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];
const RAW_RECORD_HEADER_LEN: u64 = 10;
const RAW_READ_THROUGH_GAP: u64 = 64 * 1024;

pub struct StorageManager {
    base_path: PathBuf,
    current_hour: (i32, u32, u32, u32), // Year, Month, Day, Hour
    raw_file: Option<File>,
    subdirs: SipFlowSubdirs,
    flusher_tx: Option<mpsc::UnboundedSender<FlushCommand>>,
    dropped: Arc<AtomicU64>,
}

pub struct ProcessedPacket {
    pub msg_type: MsgType,
    pub callid: Option<String>,
    pub src: String,
    pub dst: String,
    pub leg: Option<i32>, // 0=LegA, 1=LegB for RTP, None for SIP
    pub timestamp: u64,
    pub payload: Bytes,
    pub orig_size: usize,
    pub comp_size: usize,
}

#[derive(sqlx::FromRow)]
struct MediaPacketRow {
    leg: i32,
    src: String,
    timestamp: i64,
    offset: i64,
    size: i64,
}

pub(crate) struct StoredMediaPacket {
    pub leg: i32,
    pub src: String,
    pub timestamp: u64,
    pub payload: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct SipPacketRow {
    src: String,
    dst: String,
    timestamp: i64,
    offset: i64,
    size: i64,
}

#[derive(sqlx::FromRow)]
pub(crate) struct MediaSourceRow {
    pub leg: i32,
    pub src: String,
}

/// Minimum payload size worth gzip-compressing.
pub const COMPRESS_MIN_SIZE: usize = 96;

/// Default gzip compression level for stored payloads.
pub const DEFAULT_COMPRESS_LEVEL: u32 = 6;

/// Gzip-compress `payload` at `level` (clamped to 0-9) when it is large
/// enough and not already compressed (idempotent: gzip/zstd payloads pass
/// through unchanged).
///
/// Callers may invoke this early — e.g. on receiver threads to spread
/// compression across cores — and `process_packet` will not re-compress.
pub fn maybe_compress_payload(payload: Bytes, level: u32) -> Bytes {
    if payload.len() < COMPRESS_MIN_SIZE
        || payload.starts_with(&GZIP_MAGIC)
        || payload.starts_with(&ZSTD_MAGIC)
    {
        return payload;
    }
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level.min(9)));
    if encoder.write_all(&payload).is_ok()
        && let Ok(data) = encoder.finish()
    {
        return data.into();
    }
    payload
}

pub fn process_packet(packet: Packet) -> ProcessedPacket {
    process_packet_with(packet, Some(DEFAULT_COMPRESS_LEVEL))
}

/// Like [`process_packet`], with explicit compression control:
/// `Some(level)` gzip-compresses large payloads at that level, `None`
/// stores payloads uncompressed. Both forms are always readable — the read
/// path auto-detects compression from magic bytes.
pub fn process_packet_with(packet: Packet, compress: Option<u32>) -> ProcessedPacket {
    let Packet {
        msg_type,
        src,
        dst,
        timestamp,
        call_id,
        leg,
        payload,
    } = packet;
    let mut callid = call_id;
    let src_addr = format!("{}:{}", src.0, src.1);
    let dst_addr = format!("{}:{}", dst.0, dst.1);

    if matches!(msg_type, MsgType::Sip) && callid.is_none() {
        callid = extract_callid(&payload);
    }

    let orig_size = payload.len();
    let payload = match compress {
        Some(level) => maybe_compress_payload(payload, level),
        None => payload,
    };
    let comp_size = payload.len();

    ProcessedPacket {
        msg_type,
        callid,
        src: src_addr,
        dst: dst_addr,
        leg,
        timestamp,
        payload,
        orig_size,
        comp_size,
    }
}

fn seek_or_read_through(
    raw_file: &mut File,
    current_pos: &mut Option<u64>,
    target_pos: u64,
) -> std::io::Result<()> {
    if let Some(pos) = *current_pos
        && pos <= target_pos
        && target_pos - pos <= RAW_READ_THROUGH_GAP
    {
        let mut remaining = target_pos - pos;
        let mut discard = [0u8; 8192];
        while remaining > 0 {
            let len = remaining.min(discard.len() as u64) as usize;
            raw_file.read_exact(&mut discard[..len])?;
            remaining -= len as u64;
        }
        *current_pos = Some(target_pos);
        return Ok(());
    }

    raw_file.seek(SeekFrom::Start(target_pos))?;
    *current_pos = Some(target_pos);
    Ok(())
}

fn read_raw_payload(
    raw_file: &mut File,
    current_pos: &mut Option<u64>,
    offset: u64,
    size: usize,
) -> std::io::Result<Vec<u8>> {
    let payload_offset = offset + RAW_RECORD_HEADER_LEN;
    seek_or_read_through(raw_file, current_pos, payload_offset)?;

    let mut buf = vec![0u8; size];
    raw_file.read_exact(&mut buf)?;
    *current_pos = Some(payload_offset + size as u64);

    if buf.starts_with(&ZSTD_MAGIC) {
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(&buf[..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let mut result = Vec::new();
        decoder.read_to_end(&mut result)?;
        Ok(result)
    } else if buf.starts_with(&GZIP_MAGIC) {
        let mut decoder = flate2::read::GzDecoder::new(&buf[..]);
        let mut result = Vec::new();
        decoder.read_to_end(&mut result)?;
        Ok(result)
    } else {
        Ok(buf)
    }
}

impl StorageManager {
    fn datetime_to_storage_ts(dt: DateTime<Local>) -> i64 {
        dt.timestamp_micros()
    }

    pub(crate) fn new(
        base_path: &Path,
        subdirs: SipFlowSubdirs,
        flusher_tx: Option<mpsc::UnboundedSender<FlushCommand>>,
        dropped: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
            current_hour: (0, 0, 0, 0),
            raw_file: None,
            subdirs,
            flusher_tx,
            dropped: dropped.unwrap_or_default(),
        }
    }

    pub async fn write_processed(&mut self, processed: ProcessedPacket) -> Result<()> {
        let dt = Local::now();
        let h = (dt.year(), dt.month(), dt.day(), dt.hour());

        if self.raw_file.is_none() || self.current_hour != h {
            self.rotate(dt).await?;
            self.current_hour = h;
        }

        let file = self
            .raw_file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("raw_file not initialized after rotate"))?;
        let offset = file.metadata()?.len();

        file.write_all(&0x5346u16.to_be_bytes())?; // Magic
        file.write_all(&(processed.orig_size as u32).to_be_bytes())?;
        file.write_all(&(processed.comp_size as u32).to_be_bytes())?;
        file.write_all(&processed.payload)?;

        if let Some(ref tx) = self.flusher_tx {
            let meta = FlushMeta {
                msg_type: processed.msg_type,
                callid: processed.callid,
                src: processed.src,
                dst: processed.dst,
                leg: processed.leg,
                timestamp: processed.timestamp,
                offset,
                size: processed.comp_size,
            };
            if tx.send(FlushCommand::Meta(meta)).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        Ok(())
    }

    /// Number of items dropped due to flusher channel being full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub async fn check_flush(&mut self) -> Result<()> {
        if let Some(ref tx) = self.flusher_tx {
            let _ = tx.send(FlushCommand::Flush);
        }
        Ok(())
    }

    /// Force-flush all pending items and wait for completion.
    pub async fn force_flush(&mut self) -> Result<()> {
        if let Some(ref tx) = self.flusher_tx {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            if tx.send(FlushCommand::FlushSync { done: done_tx }).is_ok() {
                let _ = done_rx.await;
            }
        }
        Ok(())
    }

    async fn rotate(&mut self, dt: DateTime<Local>) -> Result<()> {
        let subdir = match self.subdirs {
            SipFlowSubdirs::Hourly => format!(
                "{:04}{:02}{:02}/{:02}",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour()
            ),
            SipFlowSubdirs::Daily => format!("{:04}{:02}{:02}", dt.year(), dt.month(), dt.day()),
            SipFlowSubdirs::None => String::new(),
        };
        let dir = self.base_path.join(subdir);
        std::fs::create_dir_all(&dir)?;

        let db_path = dir.join("sipflow.db");
        let raw_path = dir.join("data.raw");

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(raw_path)?;

        self.raw_file = Some(file);

        if let Some(ref tx) = self.flusher_tx {
            let _ = tx.send(FlushCommand::Rotate { db_path });
        }

        Ok(())
    }

    /// Apply read-optimisation PRAGMAs to a query connection.
    async fn configure_read_conn(conn: &mut SqliteConnection) {
        for pragma in [
            "PRAGMA mmap_size=268435456",
            "PRAGMA cache_size=-64000",
            "PRAGMA busy_timeout=5000",
            "PRAGMA query_only=1",
        ] {
            let _ = sqlx::query(pragma).execute(&mut *conn).await;
        }
    }

    pub async fn query_flow(
        &mut self,
        callid: &str,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let mut results = Vec::new();
        let start_ts = Self::datetime_to_storage_ts(start_dt);
        let end_ts = Self::datetime_to_storage_ts(end_dt);
        let folders = self.get_folders_in_range(start_dt, end_dt);

        for dir in folders {
            let db_path = dir.join("sipflow.db");
            let raw_path = dir.join("data.raw");

            if !db_path.exists() || !raw_path.exists() {
                continue;
            }

            let mut conn =
                SqliteConnection::connect(&format!("sqlite:{}", db_path.to_string_lossy())).await?;
            Self::configure_read_conn(&mut conn).await;
            let mut raw_file = File::open(raw_path)?;
            let mut current_pos = None;

            // Query using JOIN with call_meta
            let rows = sqlx::query_as::<_, SipPacketRow>(
                "SELECT s.src AS src,
                        s.dst AS dst,
                        s.timestamp AS timestamp,
                        s.offset AS offset,
                        s.size AS size
                 FROM sip_msgs s
                 JOIN call_meta c ON s.call_id = c.id
                  WHERE c.callid = ?
                  AND s.timestamp >= ?
                  AND s.timestamp <= ?
                 ORDER BY s.offset ASC",
            )
            .bind(callid)
            .bind(start_ts)
            .bind(end_ts)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                let offset = u64::try_from(row.offset)?;
                let size = usize::try_from(row.size)?;
                let raw_msg = read_raw_payload(&mut raw_file, &mut current_pos, offset, size)?;

                results.push(SipFlowItem {
                    src_addr: row.src,
                    dst_addr: row.dst,
                    timestamp: row.timestamp as u64,
                    payload: Bytes::from(raw_msg),
                    msg_type: SipFlowMsgType::Sip,
                    seq: 0,
                    leg: None,
                });
            }
        }
        results.sort_by_key(|r| r.timestamp);
        Ok(results)
    }

    pub async fn query_flow_in_range(
        &mut self,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let mut results = Vec::new();
        let start_ts = Self::datetime_to_storage_ts(start_dt);
        let end_ts = Self::datetime_to_storage_ts(end_dt);
        let folders = self.get_folders_in_range(start_dt, end_dt);

        for dir in folders {
            let db_path = dir.join("sipflow.db");
            let raw_path = dir.join("data.raw");

            if !db_path.exists() || !raw_path.exists() {
                continue;
            }

            let mut conn =
                SqliteConnection::connect(&format!("sqlite:{}", db_path.to_string_lossy())).await?;
            Self::configure_read_conn(&mut conn).await;
            let mut raw_file = File::open(raw_path)?;
            let mut current_pos = None;

            let rows = sqlx::query_as::<_, SipPacketRow>(
                "SELECT s.src AS src,
                        s.dst AS dst,
                        s.timestamp AS timestamp,
                        s.offset AS offset,
                        s.size AS size
                 FROM sip_msgs s
                  WHERE s.timestamp >= ?
                  AND s.timestamp <= ?
                 ORDER BY s.offset ASC",
            )
            .bind(start_ts)
            .bind(end_ts)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                let offset = u64::try_from(row.offset)?;
                let size = usize::try_from(row.size)?;
                let raw_msg = read_raw_payload(&mut raw_file, &mut current_pos, offset, size)?;

                results.push(SipFlowItem {
                    src_addr: row.src,
                    dst_addr: row.dst,
                    timestamp: row.timestamp as u64,
                    payload: Bytes::from(raw_msg),
                    msg_type: SipFlowMsgType::Sip,
                    seq: 0,
                    leg: None,
                });
            }
        }

        results.sort_by_key(|r| r.timestamp);
        Ok(results)
    }

    pub async fn query_media_stats(
        &mut self,
        callid: &str,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<SipFlowMediaStats>> {
        let mut results = std::collections::HashMap::new();
        for packet in self.query_media_packets(callid, start_dt, end_dt).await? {
            let header = parse_rtp_stats_header(&packet.payload);
            let key = (packet.leg, packet.src.clone(), header.map(|h| h.ssrc));
            results
                .entry(key)
                .or_insert_with(|| {
                    MediaStatsAccumulator::new(packet.leg, packet.src, header.map(|h| h.ssrc))
                })
                .observe(packet.timestamp as u64, header);
        }

        Ok(results
            .into_iter()
            .map(|(_, accumulator)| accumulator.into_stats())
            .collect())
    }

    pub(crate) async fn query_media_sources(
        &mut self,
        callid: &str,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<MediaSourceRow>> {
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let start_ts = Self::datetime_to_storage_ts(start_dt);
        let end_ts = Self::datetime_to_storage_ts(end_dt);
        let folders = self.get_folders_in_range(start_dt, end_dt);

        for dir in folders {
            let db_path = dir.join("sipflow.db");

            if !db_path.exists() {
                continue;
            }

            let mut conn =
                SqliteConnection::connect(&format!("sqlite:{}", db_path.to_string_lossy())).await?;
            Self::configure_read_conn(&mut conn).await;

            let mut rows = sqlx::query_as::<_, MediaSourceRow>(
                "SELECT m.leg AS leg,
                        m.src AS src
                 FROM media_msgs m
                 JOIN call_meta c ON m.call_id = c.id
                 WHERE c.callid = ?
                 AND m.timestamp >= ?
                 AND m.timestamp <= ?
                 ORDER BY m.timestamp ASC, m.id ASC",
            )
            .bind(callid)
            .bind(start_ts)
            .bind(end_ts)
            .fetch(&mut conn);

            while let Some(row) = rows.try_next().await? {
                if seen.insert((row.leg, row.src.clone())) {
                    results.push(row);
                }
            }
        }

        Ok(results)
    }

    pub(crate) async fn query_media_packets(
        &mut self,
        callid: &str,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<StoredMediaPacket>> {
        let mut results = Vec::new();
        let start_ts = Self::datetime_to_storage_ts(start_dt);
        let end_ts = Self::datetime_to_storage_ts(end_dt);
        let folders = self.get_folders_in_range(start_dt, end_dt);

        for dir in folders {
            let db_path = dir.join("sipflow.db");
            let raw_path = dir.join("data.raw");

            if !db_path.exists() || !raw_path.exists() {
                continue;
            }

            let mut conn =
                SqliteConnection::connect(&format!("sqlite:{}", db_path.to_string_lossy())).await?;
            Self::configure_read_conn(&mut conn).await;
            let mut raw_file = File::open(raw_path)?;
            let mut current_pos = None;

            let rows = sqlx::query_as::<_, MediaPacketRow>(
                "SELECT s.leg AS leg,
                        s.src AS src,
                        s.timestamp AS timestamp,
                        s.offset AS offset,
                        s.size AS size
                 FROM media_msgs s
                 JOIN call_meta c ON s.call_id = c.id
                 WHERE c.callid = ?
                 AND s.timestamp >= ?
                 AND s.timestamp <= ?
                 ORDER BY s.offset ASC",
            )
            .bind(callid)
            .bind(start_ts)
            .bind(end_ts)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                let offset = u64::try_from(row.offset)?;
                let size = usize::try_from(row.size)?;
                let payload = read_raw_payload(&mut raw_file, &mut current_pos, offset, size)?;

                results.push(StoredMediaPacket {
                    leg: row.leg,
                    src: row.src,
                    timestamp: row.timestamp as u64,
                    payload,
                });
            }
        }

        Ok(results)
    }

    pub async fn query_media(
        &mut self,
        callid: &str,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<(i32, u64, Vec<u8>)>> {
        Ok(self
            .query_media_packets(callid, start_dt, end_dt)
            .await?
            .into_iter()
            .map(|packet| (packet.leg, packet.timestamp, packet.payload))
            .collect())
    }

    fn get_folders_in_range(&self, start: DateTime<Local>, end: DateTime<Local>) -> Vec<PathBuf> {
        discover_data_dirs(&self.base_path, &self.subdirs, start, end)
    }
}

fn parse_day_dir_name(name: &str) -> Option<chrono::NaiveDate> {
    if name.len() != 8 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    chrono::NaiveDate::parse_from_str(name, "%Y%m%d").ok()
}

fn parse_hour_dir_name(name: &str) -> Option<u32> {
    if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse::<u32>().ok().filter(|h| *h < 24)
}

/// Discover on-disk data directories that may contain packets for
/// `[start, end]`.
///
/// Unlike a purely computed date-range enumeration, this scans the actual
/// directory tree. A bucket directory named for one day may contain data
/// spanning several days (e.g. replayed traffic, delayed packets, or a
/// changed `subdirs` layout), so directories outside the requested range are
/// also returned — placed after the in-range ones. Every query filters rows
/// by timestamp anyway, so scanning extra directories is safe.
///
/// Both daily (`YYYYMMDD`) and hourly (`YYYYMMDD/HH`) layouts are always
/// discovered regardless of the configured `subdirs`, and the base directory
/// itself is included last so data written with `subdirs = none` (or before a
/// layout change) remains queryable.
pub(crate) fn discover_data_dirs(
    base: &Path,
    subdirs: &SipFlowSubdirs,
    start: DateTime<Local>,
    end: DateTime<Local>,
) -> Vec<PathBuf> {
    if matches!(subdirs, SipFlowSubdirs::None) {
        return vec![base.to_path_buf()];
    }

    let start_day = start.date_naive();
    let end_day = end.date_naive();
    let start_hour = start.hour();
    let end_hour = end.hour();

    let day_in_range = |d: chrono::NaiveDate| d >= start_day && d <= end_day;
    let hour_in_range = |d: chrono::NaiveDate, h: u32| {
        if d < start_day || d > end_day {
            return false;
        }
        if d == start_day && h < start_hour {
            return false;
        }
        if d == end_day && h > end_hour {
            return false;
        }
        true
    };

    let mut in_range = Vec::new();
    let mut out_of_range = Vec::new();

    let mut days: Vec<(chrono::NaiveDate, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(base) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(date) = parse_day_dir_name(name) {
                days.push((date, path));
            }
        }
    }
    days.sort_by_key(|(d, _)| *d);

    for (date, day_path) in days {
        if day_in_range(date) {
            in_range.push(day_path.clone());
        } else {
            out_of_range.push(day_path.clone());
        }

        let mut hours: Vec<(u32, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&day_path) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if let Some(hour) = parse_hour_dir_name(name) {
                    hours.push((hour, path));
                }
            }
        }
        hours.sort_by_key(|(h, _)| *h);
        for (hour, hour_path) in hours {
            if hour_in_range(date, hour) {
                in_range.push(hour_path);
            } else {
                out_of_range.push(hour_path);
            }
        }
    }

    in_range.extend(out_of_range);
    in_range.push(base.to_path_buf());
    in_range
}

pub fn extract_callid(payload: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(payload);
    for line in s.lines() {
        if line.to_lowercase().starts_with("call-id:") {
            return Some(line[8..].trim().to_string());
        } else if line.to_lowercase().starts_with("i:") {
            // compact form
            return Some(line[2..].trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sipflow::flusher::SipFlowFlusher;
    use chrono::TimeZone;
    use std::net::IpAddr;

    fn local_dt_from_micros(ts_micros: i64) -> DateTime<Local> {
        Local
            .timestamp_micros(ts_micros)
            .single()
            .expect("valid local datetime")
    }

    async fn new_test_storage(base_path: &Path) -> (StorageManager, SipFlowFlusher) {
        let flusher = SipFlowFlusher::new(1000, 3600, 128);
        let storage = StorageManager::new(
            base_path,
            SipFlowSubdirs::None,
            Some(flusher.sender()),
            Some(flusher.dropped_count()),
        );
        (storage, flusher)
    }

    fn make_sip_processed(ts_micros: u64, call_id: &str) -> ProcessedPacket {
        let payload = format!(
            "INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {}\r\n",
            call_id
        );
        process_packet(Packet {
            msg_type: MsgType::Sip,
            src: (IpAddr::from([127, 0, 0, 1]), 5060),
            dst: (IpAddr::from([127, 0, 0, 2]), 5060),
            timestamp: ts_micros,
            call_id: None,
            leg: None,
            payload: Bytes::from(payload),
        })
    }

    fn make_rtp_processed(
        ts_micros: u64,
        call_id: &str,
        leg: i32,
        src: &str,
        payload: &[u8],
    ) -> ProcessedPacket {
        let mut packet = process_packet(Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([127, 0, 0, 1]), 30000),
            dst: (IpAddr::from([127, 0, 0, 2]), 30002),
            timestamp: ts_micros,
            call_id: None,
            leg: None,
            payload: Bytes::from(payload.to_vec()),
        });
        packet.callid = Some(call_id.to_string());
        packet.leg = Some(leg);
        packet.src = src.to_string();
        packet
    }

    #[test]
    fn test_extract_callid() {
        let msg = b"INVITE sip:test@example.com SIP/2.0\r\nCall-ID: inprocess-test-123\r\n";
        let callid = extract_callid(msg);
        assert_eq!(callid, Some("inprocess-test-123".to_string()));

        let msg2 = b"INVITE sip:test@example.com SIP/2.0\r\ni: compact-form-id\r\n";
        let callid2 = extract_callid(msg2);
        assert_eq!(callid2, Some("compact-form-id".to_string()));
    }

    #[test]
    fn test_process_packet_applies_rtp_metadata() {
        let rtp = Bytes::from_static(b"\x80\x00\x00\x2a\x00\x00\x00\xa0\x00\x00\x00\x01payload");

        let processed = process_packet(Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([198, 51, 100, 10]), 5004),
            dst: (IpAddr::from([127, 0, 0, 1]), 0),
            timestamp: 123_456,
            call_id: Some("remote-call-1".to_string()),
            leg: Some(1),
            payload: rtp.clone(),
        });

        assert_eq!(processed.callid, Some("remote-call-1".to_string()));
        assert_eq!(processed.leg, Some(1));
        assert_eq!(processed.src, "198.51.100.10:5004");
        assert_eq!(processed.payload, rtp);
        assert_eq!(processed.orig_size, rtp.len());
    }

    #[tokio::test]
    async fn test_rtp_metadata_writes_queryable_media() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut storage, _flusher) = new_test_storage(dir.path()).await;
        let timestamp = chrono::Utc::now().timestamp_micros() as u64;
        let rtp = Bytes::from_static(b"\x80\x00\x00\x2a\x00\x00\x00\xa0\x00\x00\x00\x01payload");

        let processed = process_packet(Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([203, 0, 113, 10]), 6000),
            dst: (IpAddr::from([127, 0, 0, 1]), 0),
            timestamp,
            call_id: Some("remote-call-2".to_string()),
            leg: Some(0),
            payload: rtp.clone(),
        });
        storage.write_processed(processed).await.expect("write RTP");
        storage.force_flush().await.expect("flush");

        let packets = storage
            .query_media(
                "remote-call-2",
                local_dt_from_micros(timestamp as i64 - 1),
                local_dt_from_micros(timestamp as i64 + 1),
            )
            .await
            .expect("query media");

        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].0, 0);
        assert_eq!(packets[0].1, timestamp);
        assert_eq!(packets[0].2, rtp.to_vec());
    }

    #[tokio::test]
    async fn test_query_flow_respects_time_range_inclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut storage, _flusher) = new_test_storage(dir.path()).await;

        let call_id = "flow-range-test";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 1_000) as u64;
        let t1 = (base + 2_000) as u64;
        let t2 = (base + 3_000) as u64;

        storage
            .write_processed(make_sip_processed(t0, call_id))
            .await
            .expect("write t0");
        storage
            .write_processed(make_sip_processed(t1, call_id))
            .await
            .expect("write t1");
        storage
            .write_processed(make_sip_processed(t2, call_id))
            .await
            .expect("write t2");
        storage.force_flush().await.expect("flush");

        let items = storage
            .query_flow(
                call_id,
                local_dt_from_micros(t1 as i64),
                local_dt_from_micros(t1 as i64),
            )
            .await
            .expect("query flow");

        assert_eq!(items.len(), 1, "expected only one item in narrow range");
        assert_eq!(items[0].timestamp, t1);
    }

    #[tokio::test]
    async fn test_query_media_and_stats_filter_receive_timestamp_within_selected_folders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut storage, _flusher) = new_test_storage(dir.path()).await;

        let call_id = "media-range-test";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 10_000) as u64;
        let t1 = (base + 20_000) as u64;
        let t2 = (base + 30_000) as u64;

        let p0 = b"rtp-payload-0";
        let p1 = b"rtp-payload-1";
        let p2 = b"rtp-payload-2";

        storage
            .write_processed(make_rtp_processed(t0, call_id, 0, "127.0.0.1:4000", p0))
            .await
            .expect("write t0");
        storage
            .write_processed(make_rtp_processed(t1, call_id, 0, "127.0.0.1:4000", p1))
            .await
            .expect("write t1");
        storage
            .write_processed(make_rtp_processed(t2, call_id, 0, "127.0.0.1:4000", p2))
            .await
            .expect("write t2");
        storage.force_flush().await.expect("flush");

        let start = local_dt_from_micros((t1 as i64) - 1);
        let end = local_dt_from_micros((t1 as i64) + 1);

        let packets = storage
            .query_media(call_id, start, end)
            .await
            .expect("query media");
        assert_eq!(
            packets.len(),
            1,
            "expected only media packets in the receive timestamp range"
        );
        assert_eq!(packets[0].1, t1);
        assert_eq!(packets[0].2, p1.to_vec(), "payload should match t1 packet");

        let stats = storage
            .query_media_stats(
                call_id,
                local_dt_from_micros((t1 as i64) - 1),
                local_dt_from_micros((t1 as i64) + 1),
            )
            .await
            .expect("query media stats");

        assert_eq!(stats.len(), 1, "expected one (leg,src) stats row");
        assert_eq!(stats[0].leg, 0);
        assert_eq!(
            stats[0].packet_count, 1,
            "expected only packets in the receive timestamp range"
        );

        let sources = storage
            .query_media_sources(call_id, start, end)
            .await
            .expect("query media sources");
        assert_eq!(sources.len(), 1, "expected one unique media source");
        assert_eq!(sources[0].leg, 0);
        assert_eq!(sources[0].src, "127.0.0.1:4000");
    }

    #[tokio::test]
    async fn test_daily_dir_containing_other_days_data_is_still_queryable() {
        // A "daily" bucket directory may contain data whose timestamps
        // belong to other days (replayed traffic, delayed packets, moved
        // data). Queries must still find it.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();

        // Write today's data into a bucket named for a completely
        // different day, using a None-subdirs writer rooted at that bucket.
        let stale_bucket = base.join("20200101");
        std::fs::create_dir_all(&stale_bucket).expect("mkdir");
        let (mut writer, _flusher) = new_test_storage(&stale_bucket).await;

        let call_id = "cross-day-test";
        let ts = chrono::Utc::now().timestamp_micros() as u64;
        writer
            .write_processed(make_sip_processed(ts, call_id))
            .await
            .expect("write");
        writer.force_flush().await.expect("flush");

        // Query with Daily subdirs for today's range — the 20200101 bucket
        // is outside the computed range but must still be scanned.
        let mut reader = StorageManager::new(base, SipFlowSubdirs::Daily, None, None);
        let items = reader
            .query_flow(
                call_id,
                local_dt_from_micros(ts as i64 - 1),
                local_dt_from_micros(ts as i64 + 1),
            )
            .await
            .expect("query flow");

        assert_eq!(items.len(), 1, "data in out-of-range day dir must be found");
        assert_eq!(items[0].timestamp, ts);
    }

    #[tokio::test]
    async fn test_daily_query_finds_legacy_data_in_base_dir() {
        // Data written with subdirs=None lives directly in the base dir;
        // after switching to Daily it must remain queryable.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();

        let (mut writer, _flusher) = new_test_storage(base).await;
        let call_id = "legacy-base-test";
        let ts = chrono::Utc::now().timestamp_micros() as u64;
        writer
            .write_processed(make_sip_processed(ts, call_id))
            .await
            .expect("write");
        writer.force_flush().await.expect("flush");

        let mut reader = StorageManager::new(base, SipFlowSubdirs::Daily, None, None);
        let items = reader
            .query_flow(
                call_id,
                local_dt_from_micros(ts as i64 - 1),
                local_dt_from_micros(ts as i64 + 1),
            )
            .await
            .expect("query flow");

        assert_eq!(items.len(), 1, "legacy base-dir data must be found");
    }

    #[test]
    fn test_discover_data_dirs_orders_in_range_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        std::fs::create_dir_all(base.join("20200101")).unwrap();
        std::fs::create_dir_all(base.join("20200102/05")).unwrap();
        std::fs::create_dir_all(base.join("20200103")).unwrap();
        std::fs::create_dir_all(base.join("not-a-date")).unwrap();

        let start = Local.with_ymd_and_hms(2020, 1, 2, 0, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2020, 1, 2, 23, 59, 59).unwrap();

        let dirs = discover_data_dirs(base, &SipFlowSubdirs::Daily, start, end);

        assert_eq!(dirs[0], base.join("20200102"));
        assert_eq!(dirs[1], base.join("20200102/05"));
        assert!(dirs.contains(&base.join("20200101")));
        assert!(dirs.contains(&base.join("20200103")));
        assert_eq!(dirs.last().unwrap(), &base.to_path_buf());
        assert!(!dirs.contains(&base.join("not-a-date")));
    }

    #[test]
    fn test_maybe_compress_payload_is_idempotent() {
        let original = Bytes::from(vec![b'a'; 500]);
        let once = maybe_compress_payload(original.clone(), DEFAULT_COMPRESS_LEVEL);
        assert!(once.starts_with(&GZIP_MAGIC), "large payload compressed");
        let twice = maybe_compress_payload(once.clone(), DEFAULT_COMPRESS_LEVEL);
        assert_eq!(once, twice, "already-compressed payload must pass through");

        let small = Bytes::from_static(b"tiny");
        assert_eq!(
            maybe_compress_payload(small.clone(), DEFAULT_COMPRESS_LEVEL),
            small
        );
    }

    #[tokio::test]
    async fn test_uncompressed_payload_roundtrips() {
        // With compression disabled the payload is stored as-is and the
        // read path must return it unchanged (magic-byte detection sees no
        // gzip/zstd header and passes the raw bytes through).
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut storage, _flusher) = new_test_storage(dir.path()).await;

        let call_id = "no-compress-test";
        let ts = chrono::Utc::now().timestamp_micros() as u64;
        let sip_payload = format!(
            "INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {}\r\nX-Pad: {}\r\n",
            call_id,
            "p".repeat(200)
        );

        let processed = process_packet_with(
            Packet {
                msg_type: MsgType::Sip,
                src: (IpAddr::from([127, 0, 0, 1]), 5060),
                dst: (IpAddr::from([127, 0, 0, 2]), 5060),
                timestamp: ts,
                call_id: None,
                leg: None,
                payload: Bytes::from(sip_payload.clone()),
            },
            None,
        );
        assert_eq!(processed.callid, Some(call_id.to_string()));
        assert!(
            !processed.payload.starts_with(&GZIP_MAGIC),
            "payload must be stored uncompressed"
        );
        assert_eq!(processed.comp_size, processed.orig_size);

        storage.write_processed(processed).await.expect("write");
        storage.force_flush().await.expect("flush");

        let items = storage
            .query_flow(
                call_id,
                local_dt_from_micros(ts as i64 - 1),
                local_dt_from_micros(ts as i64 + 1),
            )
            .await
            .expect("query flow");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].payload, Bytes::from(sip_payload));
    }

    #[tokio::test]
    async fn test_precompressed_sip_payload_with_explicit_callid_roundtrips() {
        // Receiver threads may compress payloads before handing them to the
        // storage path; the Call-ID is extracted before compression and
        // passed explicitly. process_packet must not re-compress, and the
        // read path must return the original uncompressed payload.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut storage, _flusher) = new_test_storage(dir.path()).await;

        let call_id = "precompressed-test";
        let ts = chrono::Utc::now().timestamp_micros() as u64;
        let sip_payload = format!(
            "INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {}\r\nX-Pad: {}\r\n",
            call_id,
            "p".repeat(200)
        );

        let compressed =
            maybe_compress_payload(Bytes::from(sip_payload.clone()), DEFAULT_COMPRESS_LEVEL);
        assert!(compressed.starts_with(&GZIP_MAGIC));

        let processed = process_packet(Packet {
            msg_type: MsgType::Sip,
            src: (IpAddr::from([127, 0, 0, 1]), 5060),
            dst: (IpAddr::from([127, 0, 0, 2]), 5060),
            timestamp: ts,
            call_id: Some(call_id.to_string()),
            leg: None,
            payload: compressed.clone(),
        });
        assert_eq!(processed.callid, Some(call_id.to_string()));
        assert_eq!(
            processed.payload, compressed,
            "must not double-compress a pre-compressed payload"
        );

        storage.write_processed(processed).await.expect("write");
        storage.force_flush().await.expect("flush");

        let items = storage
            .query_flow(
                call_id,
                local_dt_from_micros(ts as i64 - 1),
                local_dt_from_micros(ts as i64 + 1),
            )
            .await
            .expect("query flow");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].payload,
            Bytes::from(sip_payload),
            "read path must return the original payload"
        );
    }

    /// Reproduction: `query_flow_in_range` loads ALL calls' SIP data in the time
    /// range, not just one call.  When `build_payload_maps` (called during WAV
    /// generation) uses this function, it loads every concurrent call's SIP
    /// messages into memory — a major contributor to OOM under crash.
    #[tokio::test]
    async fn repro_query_flow_in_range_loads_all_calls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut storage, _flusher) = new_test_storage(dir.path()).await;

        let base = chrono::Utc::now().timestamp_micros() as u64;

        // Simulate 50 concurrent calls, each with 10 SIP messages.
        for i in 0..50u64 {
            let call_id = format!("call-{i}");
            for j in 0..10u64 {
                storage
                    .write_processed(make_sip_processed(base + i * 100 + j, &call_id))
                    .await
                    .expect("write sip");
            }
        }

        // Write 5 RTP packets for only ONE call.
        for j in 0..5u64 {
            storage
                .write_processed(make_rtp_processed(
                    base + j,
                    "call-0",
                    0,
                    "127.0.0.1:4000",
                    b"\x80\x00\x00\x01\x00\x00\x00\x01\x00\x00\x00\x01payload",
                ))
                .await
                .expect("write rtp");
        }
        storage.force_flush().await.expect("flush");

        let start = local_dt_from_micros(base as i64 - 1);
        let end = local_dt_from_micros(base as i64 + 10_000);

        // query_flow WITH call_id filter → only this call's SIP messages.
        let one_call = storage
            .query_flow("call-0", start, end)
            .await
            .expect("query_flow");
        assert_eq!(
            one_call.len(),
            10,
            "query_flow returns only call-0's 10 SIP msgs"
        );

        // query_flow_in_range WITHOUT call_id filter → ALL calls' SIP messages!
        let all_calls = storage
            .query_flow_in_range(start, end)
            .await
            .expect("query_flow_in_range");
        assert_eq!(
            all_calls.len(),
            500,
            "query_flow_in_range loads ALL 50 calls × 10 msgs = 500 items"
        );

        // ── The bug ──────────────────────────────────────────────
        // build_payload_maps only needs call-0's SIP messages (10 items)
        // to determine codecs, but query_flow_in_range loads 500 items
        // — 50× more data than necessary into memory.
        // With hundreds of concurrent calls this difference is enormous
        // and directly contributes to the OOM crashes observed in
        // production (~3 GB RSS → kernel kill).
        // ─────────────────────────────────────────────────────────
    }
}
