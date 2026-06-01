use crate::config::SipFlowSubdirs;
use crate::sipflow::protocol::{MsgType, Packet};
use crate::sipflow::{SipFlowItem, SipFlowMediaStats, SipFlowMsgType};
use anyhow::Result;
use bytes::Bytes;
use chrono::{DateTime, Datelike, Local, Timelike};
use futures::TryStreamExt;
use lru::LruCache;
use sqlx::{ConnectOptions, Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const RAW_RECORD_HEADER_LEN: u64 = 10;
const RAW_READ_THROUGH_GAP: u64 = 64 * 1024;
const RTP_REORDER_WINDOW: u16 = 64;

pub struct StorageManager {
    base_path: PathBuf,
    current_hour: (i32, u32, u32, u32), // Year, Month, Day, Hour
    db_conn: Option<SqliteConnection>,
    raw_file: Option<File>,
    batch: Vec<Meta>,
    last_flush: std::time::Instant,
    flush_count: usize,
    flush_interval: u64,
    call_id_cache: LruCache<String, i32>,
    subdirs: SipFlowSubdirs,
}

struct Meta {
    msg_type: MsgType,
    callid: Option<String>,
    src: String,
    dst: String,
    leg: Option<i32>, // 0=LegA, 1=LegB, None for SIP messages
    timestamp: u64,
    offset: u64,
    size: usize,
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

#[derive(Debug, Clone, Copy)]
struct RtpStatsHeader {
    payload_type: u8,
    sequence_number: u16,
    rtp_timestamp: u32,
    ssrc: u32,
}

#[derive(sqlx::FromRow)]
struct MediaPacketRow {
    leg: i32,
    src: String,
    timestamp: i64,
    offset: i64,
    size: i64,
}

struct StoredMediaPacket {
    leg: i32,
    src: String,
    timestamp: u64,
    payload: Vec<u8>,
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

#[derive(Default)]
struct MediaStatsAccumulator {
    leg: i32,
    src: String,
    packet_count: usize,
    lost_packets: u64,
    ssrc: Option<u32>,
    payload_type: Option<u8>,
    clock_rate: Option<u32>,
    first_sequence: Option<u16>,
    last_sequence: Option<u16>,
    pending_missing_sequences: std::collections::BTreeSet<u16>,
    prev_arrival_micros: Option<u64>,
    prev_rtp_timestamp: Option<u32>,
    jitter_rtp_units: f64,
    jitter_samples: u64,
}

impl MediaStatsAccumulator {
    fn new(leg: i32, src: String, ssrc: Option<u32>) -> Self {
        Self {
            leg,
            src,
            ssrc,
            ..Self::default()
        }
    }

    fn observe(&mut self, arrival_micros: u64, header: Option<RtpStatsHeader>) {
        self.packet_count += 1;

        let Some(header) = header else {
            return;
        };

        self.payload_type.get_or_insert(header.payload_type);
        let clock_rate = *self
            .clock_rate
            .get_or_insert_with(|| rtp_clock_rate_for_payload_type(header.payload_type));

        self.observe_sequence(header.sequence_number);
        self.observe_jitter(arrival_micros, header.rtp_timestamp, clock_rate);
    }

    fn observe_sequence(&mut self, sequence_number: u16) {
        if self.first_sequence.is_none() {
            self.first_sequence = Some(sequence_number);
            self.last_sequence = Some(sequence_number);
            return;
        }

        let Some(last_sequence) = self.last_sequence else {
            self.last_sequence = Some(sequence_number);
            return;
        };

        let diff = sequence_number.wrapping_sub(last_sequence);
        if diff == 0 {
            return;
        }

        if diff < 0x8000 {
            if diff > 1 {
                self.defer_missing_sequences(last_sequence, sequence_number);
            }
            self.last_sequence = Some(sequence_number);
            self.expire_missing_sequences();
        } else {
            self.pending_missing_sequences.remove(&sequence_number);
        }
    }

    fn defer_missing_sequences(&mut self, previous_sequence: u16, current_sequence: u16) {
        let missing_count = current_sequence.wrapping_sub(previous_sequence) - 1;
        let buffered_count = missing_count.min(RTP_REORDER_WINDOW);

        self.lost_packets += u64::from(missing_count - buffered_count);

        let first_buffered_offset = missing_count - buffered_count + 1;
        for offset in first_buffered_offset..=missing_count {
            self.pending_missing_sequences
                .insert(previous_sequence.wrapping_add(offset));
        }
    }

    fn expire_missing_sequences(&mut self) {
        let Some(last_sequence) = self.last_sequence else {
            return;
        };

        let expired: Vec<u16> = self
            .pending_missing_sequences
            .iter()
            .copied()
            .filter(|sequence| {
                let age = last_sequence.wrapping_sub(*sequence);
                age > RTP_REORDER_WINDOW && age < 0x8000
            })
            .collect();

        self.lost_packets += expired.len() as u64;
        for sequence in expired {
            self.pending_missing_sequences.remove(&sequence);
        }
    }

    fn observe_jitter(&mut self, arrival_micros: u64, rtp_timestamp: u32, clock_rate: u32) {
        if let (Some(prev_arrival), Some(prev_rtp)) =
            (self.prev_arrival_micros, self.prev_rtp_timestamp)
        {
            let arrival_delta = arrival_micros as i128 - prev_arrival as i128;
            let arrival_delta_units =
                arrival_delta as f64 * clock_rate as f64 / 1_000_000.0;
            let rtp_delta_units = rtp_timestamp_delta(rtp_timestamp, prev_rtp) as f64;
            let delta = (arrival_delta_units - rtp_delta_units).abs();

            if delta.is_finite() {
                self.jitter_rtp_units += (delta - self.jitter_rtp_units) / 16.0;
                self.jitter_samples += 1;
            }
        }

        self.prev_arrival_micros = Some(arrival_micros);
        self.prev_rtp_timestamp = Some(rtp_timestamp);
    }

    fn into_stats(self) -> SipFlowMediaStats {
        let lost_packets = self.lost_packets + self.pending_missing_sequences.len() as u64;
        let expected_packets = self.packet_count as u64 + lost_packets;
        let loss_percent = if expected_packets > 0 {
            lost_packets as f64 / expected_packets as f64 * 100.0
        } else {
            0.0
        };
        let jitter_ms = match (self.clock_rate, self.jitter_samples > 0) {
            (Some(clock_rate), true) if clock_rate > 0 => {
                Some(self.jitter_rtp_units * 1000.0 / clock_rate as f64)
            }
            _ => None,
        };

        SipFlowMediaStats {
            leg: self.leg,
            src: self.src,
            packet_count: self.packet_count,
            lost_packets,
            expected_packets,
            loss_percent,
            jitter_ms,
            ssrc: self.ssrc,
            payload_type: self.payload_type,
            clock_rate: self.clock_rate,
        }
    }
}

fn parse_rtp_stats_header(raw: &[u8]) -> Option<RtpStatsHeader> {
    if raw.len() < 12 || raw[0] >> 6 != 2 {
        return None;
    }

    Some(RtpStatsHeader {
        payload_type: raw[1] & 0x7f,
        sequence_number: u16::from_be_bytes([raw[2], raw[3]]),
        rtp_timestamp: u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]),
        ssrc: u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]),
    })
}

fn rtp_clock_rate_for_payload_type(payload_type: u8) -> u32 {
    match payload_type {
        0 | 8 | 9 | 18 => 8000,
        96..=127 => 48000,
        _ => 8000,
    }
}

fn rtp_timestamp_delta(current: u32, previous: u32) -> i64 {
    let forward = current.wrapping_sub(previous);
    if forward <= i32::MAX as u32 {
        forward as i64
    } else {
        -(previous.wrapping_sub(current) as i64)
    }
}

pub fn process_packet(packet: Packet) -> ProcessedPacket {
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
    let payload = payload;

    if matches!(msg_type, MsgType::Sip) && callid.is_none() {
        callid = extract_callid(&payload);
    }

    let orig_size = payload.len();
    let (payload, comp_size, _compressed) = if orig_size >= 96 {
        if let Ok(data) = zstd::encode_all(&payload[..], 3) {
            let size = data.len();
            (data.into(), size, true)
        } else {
            (payload, orig_size, false)
        }
    } else {
        (payload, orig_size, false)
    };

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
        zstd::decode_all(&buf[..])
    } else {
        Ok(buf)
    }
}

impl StorageManager {
    fn datetime_to_storage_ts(dt: DateTime<Local>) -> i64 {
        dt.timestamp_micros()
    }

    pub fn new(
        base_path: &Path,
        flush_count: usize,
        flush_interval: u64,
        id_cache_size: usize,
        subdirs: SipFlowSubdirs,
    ) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
            current_hour: (0, 0, 0, 0),
            db_conn: None,
            raw_file: None,
            batch: Vec::new(),
            last_flush: std::time::Instant::now(),
            flush_count,
            flush_interval,
            call_id_cache: LruCache::new(NonZeroUsize::new(id_cache_size).unwrap()),
            subdirs,
        }
    }

    pub async fn write_processed(&mut self, processed: ProcessedPacket) -> Result<()> {
        let dt = Local::now();
        let h = (dt.year(), dt.month(), dt.day(), dt.hour());

        if self.db_conn.is_none() || self.current_hour != h {
            self.rotate(dt).await?;
            self.current_hour = h;
            self.call_id_cache.clear();
        }

        let file = self.raw_file.as_mut().unwrap();
        let offset = file.metadata()?.len();

        file.write_all(&0x5346u16.to_be_bytes())?; // Magic
        file.write_all(&(processed.orig_size as u32).to_be_bytes())?;
        file.write_all(&(processed.comp_size as u32).to_be_bytes())?;
        file.write_all(&processed.payload)?;

        self.batch.push(Meta {
            msg_type: processed.msg_type,
            callid: processed.callid,
            src: processed.src,
            dst: processed.dst,
            leg: processed.leg,
            timestamp: processed.timestamp,
            offset,
            size: processed.comp_size,
        });

        if self.batch.len() >= self.flush_count
            || self.last_flush.elapsed().as_secs() >= self.flush_interval
        {
            self.flush_batch().await?;
        }

        Ok(())
    }

    pub async fn check_flush(&mut self) -> Result<()> {
        if !self.batch.is_empty() && self.last_flush.elapsed().as_secs() >= self.flush_interval {
            self.flush_batch().await?;
        }
        Ok(())
    }

    /// Force-flush all pending items regardless of thresholds.
    pub async fn force_flush(&mut self) -> Result<()> {
        self.flush_batch().await
    }

    async fn rotate(&mut self, dt: DateTime<Local>) -> Result<()> {
        if !self.batch.is_empty() {
            self.flush_batch().await?;
        }
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

        let mut conn = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .connect()
            .await?;

        // Create call_meta table for callid mapping
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS call_meta (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                callid TEXT UNIQUE NOT NULL
            )",
        )
        .execute(&mut conn)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_callid ON call_meta(callid)")
            .execute(&mut conn)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sip_msgs (
                id INTEGER PRIMARY KEY,
                call_id INTEGER NOT NULL,
                src TEXT NOT NULL,
                dst TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                offset INTEGER NOT NULL,
                size INTEGER NOT NULL
            )",
        )
        .execute(&mut conn)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sip_call ON sip_msgs(call_id)")
            .execute(&mut conn)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS media_msgs (
                id INTEGER PRIMARY KEY,
                call_id INTEGER NOT NULL,
                leg INTEGER NOT NULL,
                src TEXT NOT NULL DEFAULT '',
                timestamp INTEGER NOT NULL,
                offset INTEGER NOT NULL,
                size INTEGER NOT NULL
            )",
        )
        .execute(&mut conn)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_media_call ON media_msgs(call_id)")
            .execute(&mut conn)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_media_call_timestamp ON media_msgs(call_id, timestamp)",
        )
        .execute(&mut conn)
        .await?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(raw_path)?;

        self.db_conn = Some(conn);
        self.raw_file = Some(file);
        Ok(())
    }

    async fn flush_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }

        if let Some(conn) = self.db_conn.as_mut() {
            let mut tx = conn.begin().await?;

            for meta in self.batch.drain(..) {
                // Get or create call_id from call_meta
                let call_id = if let Some(ref callid) = meta.callid {
                    // Check LRU cache first
                    if let Some(&cached_id) = self.call_id_cache.get(callid) {
                        cached_id
                    } else {
                        // Cache miss, query or insert into call_meta
                        let id: i32 = sqlx::query_scalar(
                            "INSERT INTO call_meta (callid) VALUES (?) 
                             ON CONFLICT(callid) DO UPDATE SET callid=callid 
                             RETURNING id",
                        )
                        .bind(callid)
                        .fetch_one(&mut *tx)
                        .await?;

                        // Update LRU cache
                        self.call_id_cache.put(callid.clone(), id);
                        id
                    }
                } else {
                    continue; // Skip records without callid
                };

                match meta.msg_type {
                    MsgType::Sip => {
                        sqlx::query(
                            "INSERT INTO sip_msgs (call_id, src, dst, timestamp, offset, size) VALUES (?, ?, ?, ?, ?, ?)",
                        )
                        .bind(call_id)
                        .bind(meta.src)
                        .bind(meta.dst)
                        .bind(meta.timestamp as i64)
                        .bind(meta.offset as i64)
                        .bind(meta.size as i64)
                        .execute(&mut *tx)
                        .await?;
                    }
                    MsgType::Rtp => {
                        let leg = meta.leg.unwrap_or(0);
                        sqlx::query(
                            "INSERT INTO media_msgs (
                                call_id,
                                leg,
                                src,
                                timestamp,
                                offset,
                                size
                            ) VALUES (?, ?, ?, ?, ?, ?)",
                        )
                        .bind(call_id)
                        .bind(leg)
                        .bind(meta.src)
                        .bind(meta.timestamp as i64)
                        .bind(meta.offset as i64)
                        .bind(meta.size as i64)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
            }
            tx.commit().await?;
        }
        if let Some(file) = self.raw_file.as_mut() {
            file.flush()?;
        }
        self.last_flush = std::time::Instant::now();
        Ok(())
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
                    MediaStatsAccumulator::new(
                        packet.leg,
                        packet.src,
                        header.map(|h| h.ssrc),
                    )
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

    async fn query_media_packets(
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
                let payload =
                    read_raw_payload(&mut raw_file, &mut current_pos, offset, size)?;

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
        let mut folders = Vec::new();
        match self.subdirs {
            SipFlowSubdirs::None => {
                folders.push(self.base_path.clone());
            }
            SipFlowSubdirs::Daily => {
                let mut curr = start.date_naive();
                let end = end.date_naive();
                while curr <= end {
                    let subdir = format!("{:04}{:02}{:02}", curr.year(), curr.month(), curr.day());
                    folders.push(self.base_path.join(subdir));
                    curr += chrono::Duration::days(1);
                }
            }
            SipFlowSubdirs::Hourly => {
                let mut curr = start
                    .with_minute(0)
                    .unwrap()
                    .with_second(0)
                    .unwrap()
                    .with_nanosecond(0)
                    .unwrap();
                let end = end
                    .with_minute(0)
                    .unwrap()
                    .with_second(0)
                    .unwrap()
                    .with_nanosecond(0)
                    .unwrap();

                while curr <= end {
                    let subdir = format!(
                        "{:04}{:02}{:02}/{:02}",
                        curr.year(),
                        curr.month(),
                        curr.day(),
                        curr.hour()
                    );
                    folders.push(self.base_path.join(subdir));
                    curr += chrono::Duration::hours(1);
                }
            }
        }
        folders
    }
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
    use chrono::TimeZone;
    use std::net::IpAddr;

    fn local_dt_from_micros(ts_micros: i64) -> DateTime<Local> {
        Local
            .timestamp_micros(ts_micros)
            .single()
            .expect("valid local datetime")
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
        let mut storage = StorageManager::new(dir.path(), 1000, 3600, 128, SipFlowSubdirs::None);
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

    #[test]
    fn test_media_stats_reordered_packet_clears_pending_loss() {
        let mut stats = MediaStatsAccumulator::new(0, "127.0.0.1:4000".to_string(), Some(1));

        for (sequence_number, rtp_timestamp, arrival_micros) in [
            (10, 1_600, 10_000),
            (12, 1_920, 30_000),
            (11, 1_760, 40_000),
        ] {
            stats.observe(
                arrival_micros,
                Some(RtpStatsHeader {
                    payload_type: 0,
                    sequence_number,
                    rtp_timestamp,
                    ssrc: 1,
                }),
            );
        }

        let stats = stats.into_stats();
        assert_eq!(stats.packet_count, 3);
        assert_eq!(stats.lost_packets, 0);
        assert_eq!(stats.expected_packets, 3);
    }

    #[test]
    fn test_media_stats_unfilled_gap_counts_as_loss() {
        let mut stats = MediaStatsAccumulator::new(0, "127.0.0.1:4000".to_string(), Some(1));

        for (sequence_number, rtp_timestamp, arrival_micros) in [
            (10, 1_600, 10_000),
            (12, 1_920, 30_000),
        ] {
            stats.observe(
                arrival_micros,
                Some(RtpStatsHeader {
                    payload_type: 0,
                    sequence_number,
                    rtp_timestamp,
                    ssrc: 1,
                }),
            );
        }

        let stats = stats.into_stats();
        assert_eq!(stats.packet_count, 2);
        assert_eq!(stats.lost_packets, 1);
        assert_eq!(stats.expected_packets, 3);
    }

    #[tokio::test]
    async fn test_query_flow_respects_time_range_inclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut storage = StorageManager::new(dir.path(), 1000, 3600, 128, SipFlowSubdirs::None);

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
        let mut storage = StorageManager::new(dir.path(), 1000, 3600, 128, SipFlowSubdirs::None);

        let call_id = "media-range-test";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 10_000) as u64;
        let t1 = (base + 20_000) as u64;
        let t2 = (base + 30_000) as u64;

        let p0 = b"rtp-payload-0";
        let p1 = b"rtp-payload-1";
        let p2 = b"rtp-payload-2";

        storage
            .write_processed(make_rtp_processed(
                t0,
                call_id,
                0,
                "127.0.0.1:4000",
                p0,
            ))
            .await
            .expect("write t0");
        storage
            .write_processed(make_rtp_processed(
                t1,
                call_id,
                0,
                "127.0.0.1:4000",
                p1,
            ))
            .await
            .expect("write t1");
        storage
            .write_processed(make_rtp_processed(
                t2,
                call_id,
                0,
                "127.0.0.1:4000",
                p2,
            ))
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
}
