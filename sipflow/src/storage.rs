use crate::protocol::{MsgType, Packet};
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};
use sqlx::{ConnectOptions, Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
}

struct Meta {
    msg_type: MsgType,
    callid: Option<String>,
    src: String,
    dst: String,
    timestamp: u64,
    offset: u64,
    size: usize,
    orig_size: usize,
}

pub struct ProcessedPacket {
    pub msg_type: MsgType,
    pub callid: Option<String>,
    pub src: String,
    pub dst: String,
    pub timestamp: u64,
    pub payload: Vec<u8>,
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
            (data, size, true)
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
        timestamp: packet.timestamp,
        payload,
        orig_size,
        comp_size,
    }
}

impl StorageManager {
    pub fn new(base_path: &Path, flush_count: usize, flush_interval: u64) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
            current_hour: (0, 0, 0, 0),
            db_conn: None,
            raw_file: None,
            batch: Vec::new(),
            last_flush: std::time::Instant::now(),
            flush_count,
            flush_interval,
        }
    }

    pub async fn write_processed(&mut self, processed: ProcessedPacket) -> Result<()> {
        let dt = Local
            .timestamp_opt(processed.timestamp as i64 / 1_000_000, 0)
            .single()
            .unwrap_or_else(Local::now);
        let h = (dt.year(), dt.month(), dt.day(), dt.hour());

        if self.db_conn.is_none() || self.current_hour != h {
            self.rotate(dt).await?;
            self.current_hour = h;
        }

        let mut file = self.raw_file.as_ref().unwrap();
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
            timestamp: processed.timestamp,
            offset,
            size: processed.comp_size,
            orig_size: processed.orig_size,
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

        let dir = self.base_path.join(format!(
            "{:04}{:02}{:02}/{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour()
        ));
        std::fs::create_dir_all(&dir)?;

        let db_path = dir.join("sipflow.db");
        let raw_path = dir.join("data.raw");

        let mut conn = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .connect()
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sip_msgs (
                id INTEGER PRIMARY KEY,
                callid TEXT,
                src TEXT,
                dst TEXT,
                timestamp INTEGER,
                offset INTEGER,
                size INTEGER,
                orig_size INTEGER
            )",
        )
        .execute(&mut conn)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sip_callid ON sip_msgs(callid)")
            .execute(&mut conn)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS media_msgs (
                id INTEGER PRIMARY KEY,
                src TEXT,
                dst TEXT,
                timestamp INTEGER,
                offset INTEGER,
                size INTEGER,
                orig_size INTEGER
            )",
        )
        .execute(&mut conn)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_media_ts ON media_msgs(timestamp)")
            .execute(&mut conn)
            .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_media_endpoints ON media_msgs(src, dst)")
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
                match meta.msg_type {
                    MsgType::Sip => {
                        sqlx::query(
                            "INSERT INTO sip_msgs (callid, src, dst, timestamp, offset, size, orig_size) VALUES (?, ?, ?, ?, ?, ?, ?)",
                        )
                        .bind(meta.callid)
                        .bind(meta.src)
                        .bind(meta.dst)
                        .bind(meta.timestamp as i64)
                        .bind(meta.offset as i64)
                        .bind(meta.size as i64)
                        .bind(meta.orig_size as i64)
                        .execute(&mut *tx)
                        .await?;
                    }
                    MsgType::Rtp => {
                        sqlx::query(
                            "INSERT INTO media_msgs (src, dst, timestamp, offset, size, orig_size) VALUES (?, ?, ?, ?, ?, ?)",
                        )
                        .bind(meta.src)
                        .bind(meta.dst)
                        .bind(meta.timestamp as i64)
                        .bind(meta.offset as i64)
                        .bind(meta.size as i64)
                        .bind(meta.orig_size as i64)
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
    ) -> Result<Vec<serde_json::Value>> {
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
                "SELECT src, dst, timestamp, offset, size, orig_size FROM sip_msgs WHERE callid = ? ORDER BY timestamp ASC",
            )
            .bind(callid)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                use sqlx::Row;
                let src: String = row.get(0);
                let dst: String = row.get(1);
                let ts: i64 = row.get(2);
                let offset: i64 = row.get(3);
                let size: i64 = row.get(4);
                let orig_size: i64 = row.get(5);

                let mut buf = vec![0u8; size as usize];
                let raw_msg = (|| -> std::io::Result<Vec<u8>> {
                    raw_file.seek(SeekFrom::Start(offset as u64 + 10))?;
                    raw_file.read_exact(&mut buf)?;

                    if size != orig_size {
                        zstd::decode_all(&buf[..])
                    } else {
                        Ok(buf)
                    }
                })()?;

                results.push(serde_json::json!({
                    "src": src,
                    "dst": dst,
                    "timestamp": ts,
                    "raw_message": String::from_utf8_lossy(&raw_msg).to_string(),
                }));
            }
        }
        results.sort_by_key(|r| r["timestamp"].as_i64().unwrap_or(0));
        Ok(results)
    }

    pub async fn query_media(
        &mut self,
        src_ip: &str,
        dst_ip: &str,
        start_ts: u64,
        end_ts: u64,
        start_dt: DateTime<Local>,
        end_dt: DateTime<Local>,
    ) -> Result<Vec<Vec<u8>>> {
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
                "SELECT offset, size, orig_size, timestamp FROM media_msgs WHERE ((src LIKE ? AND dst LIKE ?) OR (src LIKE ? AND dst LIKE ?)) AND timestamp >= ? AND timestamp <= ? ORDER BY timestamp ASC",
            )
            .bind(format!("{}%", src_ip))
            .bind(format!("{}%", dst_ip))
            .bind(format!("{}%", dst_ip))
            .bind(format!("{}%", src_ip))
            .bind(start_ts as i64)
            .bind(end_ts as i64)
            .fetch_all(&mut conn)
            .await?;

            for row in rows {
                use sqlx::Row;
                let offset: i64 = row.get(0);
                let size: i64 = row.get(1);
                let orig_size: i64 = row.get(2);
                let ts: i64 = row.get(3);

                let mut buf = vec![0u8; size as usize];
                let raw_msg = (|| -> std::io::Result<Vec<u8>> {
                    raw_file.seek(SeekFrom::Start(offset as u64 + 10))?;
                    raw_file.read_exact(&mut buf)?;
                    if size != orig_size {
                        zstd::decode_all(&buf[..])
                    } else {
                        Ok(buf)
                    }
                })()?;

                results.push((ts, raw_msg));
            }
        }
        results.sort_by_key(|(ts, _)| *ts);
        Ok(results.into_iter().map(|(_, d)| d).collect())
    }

    fn get_folders_in_range(&self, start: DateTime<Local>, end: DateTime<Local>) -> Vec<PathBuf> {
        let mut folders = Vec::new();
        let mut curr = start;
        while curr <= end {
            let dir = self.base_path.join(format!(
                "{:04}{:02}{:02}/{:02}",
                curr.year(),
                curr.month(),
                curr.day(),
                curr.hour()
            ));
            folders.push(dir);
            curr = curr + chrono::Duration::hours(1);
        }
        folders
    }
}

fn extract_callid(payload: &[u8]) -> Option<String> {
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
    use crate::protocol::{MsgType, Packet};
    use chrono::{Duration, Local, TimeZone};
    use std::io::Read;
    use std::net::IpAddr;
    use tempfile::tempdir;

    #[test]
    fn test_folders_in_range() {
        let temp = tempdir().unwrap();
        let mg = StorageManager::new(temp.path(), 100, 5);
        let start = Local.with_ymd_and_hms(2023, 10, 27, 10, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2023, 10, 27, 12, 0, 0).unwrap();
        let folders = mg.get_folders_in_range(start, end);
        assert_eq!(folders.len(), 3);
        assert!(folders[0].to_str().unwrap().contains("20231027/10"));
        assert!(folders[1].to_str().unwrap().contains("20231027/11"));
        assert!(folders[2].to_str().unwrap().contains("20231027/12"));
    }

    #[test]
    fn test_process_packet_compression() {
        // Redundant data to ensure compression
        let large_payload = vec![b'A'; 200];
        let packet = Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([127, 0, 0, 1]), 1234),
            dst: (IpAddr::from([127, 0, 0, 1]), 5678),
            timestamp: Local::now().timestamp_micros() as u64,
            payload: large_payload.clone(),
        };

        let processed = process_packet(packet);
        assert!(processed.comp_size < 200); // Should be compressed
        assert_eq!(processed.orig_size, 200);

        let small_payload = vec![b'B'; 20];
        let packet_small = Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([127, 0, 0, 1]), 1234),
            dst: (IpAddr::from([127, 0, 0, 1]), 5678),
            timestamp: Local::now().timestamp_micros() as u64,
            payload: small_payload.clone(),
        };
        let processed_small = process_packet(packet_small);
        assert_eq!(processed_small.comp_size, 20); // No compression
    }

    #[tokio::test]
    async fn test_storage_write_and_query() {
        let temp = tempdir().unwrap();
        let mut mg = StorageManager::new(temp.path(), 10, 1);

        let callid = "test-callid-123".to_string();
        let now = Local::now();
        let ts = now.timestamp_micros() as u64;

        let p1 = Packet {
            msg_type: MsgType::Sip,
            src: (IpAddr::from([10, 0, 0, 1]), 5060),
            dst: (IpAddr::from([10, 0, 0, 2]), 5060),
            timestamp: ts,
            payload: format!(
                "INVITE sip:alice@example.com SIP/2.0\r\nCall-ID: {}\r\n\r\n",
                callid
            )
            .into_bytes(),
        };

        let processed = process_packet(p1);
        mg.write_processed(processed).await.unwrap();
        mg.flush_batch().await.unwrap();

        let flow = mg
            .query_flow(&callid, now - Duration::hours(1), now + Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(flow.len(), 1);
        assert_eq!(flow[0]["src"], "10.0.0.1:5060");
        assert!(flow[0]["raw_message"].as_str().unwrap().contains("INVITE"));
    }

    #[tokio::test]
    async fn test_no_compression_small_packet() {
        let dir = tempdir().unwrap();
        let mut mg = StorageManager::new(dir.path(), 10, 5);

        let small_payload = b"small packet".to_vec();
        let now = Local::now();
        let packet = Packet {
            msg_type: MsgType::Sip,
            src: (IpAddr::from([127, 0, 0, 1]), 5060),
            dst: (IpAddr::from([127, 0, 0, 1]), 5060),
            timestamp: now.timestamp_micros() as u64,
            payload: small_payload.clone(),
        };

        let processed = process_packet(packet);
        mg.write_processed(processed).await.unwrap();
        mg.flush_batch().await.unwrap();

        // Manual verification of raw file
        let path = dir.path().join(format!(
            "{:04}{:02}{:02}/{:02}/data.raw",
            now.year(),
            now.month(),
            now.day(),
            now.hour()
        ));
        let mut f = File::open(path).expect("Raw file should exist");
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();

        // Header(10) + Payload(12) = 22
        assert_eq!(buf.len(), 10 + small_payload.len());
        // CompSize (bytes 6-10) should be 12
        assert_eq!(u32::from_be_bytes(buf[6..10].try_into().unwrap()), 12);
    }
}
