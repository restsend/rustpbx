use crate::config::SipFlowSubdirs;
use crate::sipflow::protocol::{MsgType, Packet};
use crate::sipflow::{SipFlowItem, SipFlowMsgType};
use anyhow::Result;
use bytes::Bytes;
use chrono::{DateTime, Datelike, Local, Timelike};
use lru::LruCache;
use sqlx::{ConnectOptions, Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

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

pub fn process_packet(packet: Packet) -> ProcessedPacket {
    let mut callid = None;
    if matches!(packet.msg_type, MsgType::Sip) {
        callid = extract_callid(&packet.payload);
    }

    let orig_size = packet.payload.len();
    let (payload, comp_size, _compressed) = if orig_size >= 96 {
        if let Ok(data) = zstd::encode_all(&packet.payload[..], 3) {
            let size = data.len();
            (data.into(), size, true)
        } else {
            (packet.payload, orig_size, false)
        }
    } else {
        (packet.payload, orig_size, false)
    };

    ProcessedPacket {
        msg_type: packet.msg_type,
        callid,
        src: format!("{}:{}", packet.src.0, packet.src.1),
        dst: format!("{}:{}", packet.dst.0, packet.dst.1),
        leg: None, // Will be set by caller for RTP packets
        timestamp: packet.timestamp,
        payload,
        orig_size,
        comp_size,
    }
}

impl StorageManager {
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
                            "INSERT INTO media_msgs (call_id, leg, src, timestamp, offset, size) VALUES (?, ?, ?, ?, ?, ?)",
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

            // Query using JOIN with call_meta
            let rows = sqlx::query(
                "SELECT s.src, s.dst, s.timestamp, s.offset, s.size 
                 FROM sip_msgs s
                 JOIN call_meta c ON s.call_id = c.id
                 WHERE c.callid = ? 
                 ORDER BY s.timestamp ASC",
            )
            .bind(&callid)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                use sqlx::Row;
                let src: String = row.get(0);
                let dst: String = row.get(1);
                let ts: i64 = row.get(2);
                let offset: i64 = row.get(3);
                let size: i64 = row.get(4);

                let mut buf = vec![0u8; size as usize];
                let raw_msg = (|| -> std::io::Result<Vec<u8>> {
                    raw_file.seek(SeekFrom::Start(offset as u64 + 10))?;
                    raw_file.read_exact(&mut buf)?;

                    // Try to decompress, read orig_size from data.raw header
                    raw_file.seek(SeekFrom::Start(offset as u64 + 2))?;
                    let mut orig_size_buf = [0u8; 4];
                    raw_file.read_exact(&mut orig_size_buf)?;
                    let orig_size = u32::from_be_bytes(orig_size_buf) as usize;

                    if size as usize != orig_size {
                        zstd::decode_all(&buf[..])
                    } else {
                        Ok(buf)
                    }
                })()?;

                results.push(SipFlowItem {
                    src_addr: src,
                    dst_addr: dst,
                    timestamp: ts as u64,
                    payload: Bytes::from(raw_msg),
                    msg_type: SipFlowMsgType::Sip,
                    seq: 0,
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
    ) -> Result<Vec<(i32, String, usize)>> {
        let mut results = std::collections::HashMap::new();
        let folders = self.get_folders_in_range(start_dt, end_dt);

        for dir in folders {
            let db_path = dir.join("sipflow.db");

            if !db_path.exists() {
                continue;
            }

            let mut conn =
                SqliteConnection::connect(&format!("sqlite:{}", db_path.to_string_lossy())).await?;

            let rows = sqlx::query(
                "SELECT m.leg, m.src, COUNT(*) as count 
                 FROM media_msgs m
                 JOIN call_meta c ON m.call_id = c.id
                 WHERE c.callid = ? 
                 GROUP BY m.leg, m.src",
            )
            .bind(&callid)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                use sqlx::Row;
                let leg: i32 = row.get(0);
                let src: String = row.get(1);
                let count: i64 = row.get(2);
                *results.entry((leg, src)).or_insert(0) += count as usize;
            }
        }

        Ok(results
            .into_iter()
            .map(|((leg, src), count)| (leg, src, count))
            .collect())
    }

    pub async fn query_media(
        &mut self,
        callid: &str,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<(i32, u64, Vec<u8>)>> {
        let mut results = Vec::new();
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

            let rows = sqlx::query(
                "SELECT s.offset, s.size, s.timestamp, s.leg
                 FROM media_msgs s
                 JOIN call_meta c ON s.call_id = c.id
                 WHERE c.callid = ?
                 ORDER BY s.timestamp ASC",
            )
            .bind(&callid)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                use sqlx::Row;
                let offset: i64 = row.get(0);
                let size: i64 = row.get(1);
                let ts: i64 = row.get(2);
                let leg: i32 = row.get(3);

                let mut buf = vec![0u8; size as usize];
                let raw_payload = (|| -> std::io::Result<Vec<u8>> {
                    raw_file.seek(SeekFrom::Start(offset as u64 + 10))?;
                    raw_file.read_exact(&mut buf)?;

                    // Try to decompress
                    raw_file.seek(SeekFrom::Start(offset as u64 + 2))?;
                    let mut orig_size_buf = [0u8; 4];
                    raw_file.read_exact(&mut orig_size_buf)?;
                    let orig_size = u32::from_be_bytes(orig_size_buf) as usize;

                    if size as usize != orig_size {
                        zstd::decode_all(&buf[..])
                    } else {
                        Ok(buf)
                    }
                })()?;

                results.push((leg, ts as u64, raw_payload));
            }
        }
        results.sort_by_key(|r| r.1);
        Ok(results)
    }

    fn get_folders_in_range(&self, start: DateTime<Local>, end: DateTime<Local>) -> Vec<PathBuf> {
        let mut folders = Vec::new();
        let mut curr = start;
        while curr <= end {
            let subdir = match self.subdirs {
                SipFlowSubdirs::Hourly => format!(
                    "{:04}{:02}{:02}/{:02}",
                    curr.year(),
                    curr.month(),
                    curr.day(),
                    curr.hour()
                ),
                SipFlowSubdirs::Daily => {
                    format!("{:04}{:02}{:02}", curr.year(), curr.month(), curr.day())
                }
                SipFlowSubdirs::None => String::new(),
            };
            let dir = self.base_path.join(subdir);
            folders.push(dir);
            curr = curr + chrono::Duration::hours(1);
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

    #[test]
    fn test_extract_callid() {
        let msg = b"INVITE sip:test@example.com SIP/2.0\r\nCall-ID: inprocess-test-123\r\n";
        let callid = extract_callid(msg);
        assert_eq!(callid, Some("inprocess-test-123".to_string()));

        let msg2 = b"INVITE sip:test@example.com SIP/2.0\r\ni: compact-form-id\r\n";
        let callid2 = extract_callid(msg2);
        assert_eq!(callid2, Some("compact-form-id".to_string()));
    }
}
