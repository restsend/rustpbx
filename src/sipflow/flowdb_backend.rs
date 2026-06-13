use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Local};
use flowdb::{Config as FlowDbConfig, Engine, Record, ScanRange};
use futures::FutureExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::sipflow::flowdb_codec::{
    SIP_PREFIX, decode_rtp_value, decode_sip_value, encode_rtp_value, encode_sip_value,
    make_rtp_key, make_sip_key, rtp_call_leg_prefix, rtp_call_prefix, sip_call_prefix,
};
use crate::sipflow::rtp_stats::{MediaStatsAccumulator, parse_rtp_stats_header};
use crate::sipflow::wav_utils::{
    LegPayloadTypeMap, PayloadTypeMap, build_payload_type_map, build_payload_type_map_by_leg,
    generate_wav_to_writer,
};
use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats, SipFlowMsgType};

pub struct FlowDbBackend {
    engine: Arc<Engine>,
    counter: AtomicU64,
    ttl_micros: Option<i64>,
    _maintenance: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for FlowDbBackend {
    fn drop(&mut self) {
        if let Some(handle) = self._maintenance.take() {
            handle.abort();
        }
    }
}

impl FlowDbBackend {
    pub fn new(
        data_dir: impl Into<PathBuf>,
        ttl_secs: Option<u64>,
        memtable_size_mb: usize,
        block_cache_capacity_mb: usize,
    ) -> Result<Self> {
        let config = FlowDbConfig {
            data_dir: data_dir.into(),
            default_ttl_secs: ttl_secs,
            memtable_size_mb,
            block_cache_capacity_mb,
            ..Default::default()
        };

        let engine = Engine::open(config)
            .now_or_never()
            .ok_or_else(|| anyhow::anyhow!("Engine::open did not complete synchronously"))??;

        let engine = Arc::new(engine);
        let ttl_micros = ttl_secs.map(|s| s as i64 * 1_000_000);

        let maintenance = engine.spawn_background_maintenance();

        Ok(Self {
            engine,
            counter: AtomicU64::new(0),
            ttl_micros,
            _maintenance: maintenance,
        })
    }

    fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    fn make_expire_at(&self, ts: i64) -> i64 {
        match self.ttl_micros {
            Some(ttl) => ts + ttl,
            None => i64::MAX,
        }
    }

    fn scan_sip_flow_in_range(
        &self,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<Vec<SipFlowItem>> {
        let iter = self
            .engine
            .scan(ScanRange::prefix_time_range(SIP_PREFIX, start_ts, end_ts))?;

        let mut items: Vec<SipFlowItem> = iter
            .filter_map(|result| {
                let rec = result.ok()?;
                let (src, dst, payload) = decode_sip_value(&rec.value).ok()?;
                Some(SipFlowItem {
                    timestamp: rec.ts as u64,
                    seq: 0,
                    leg: None,
                    msg_type: SipFlowMsgType::Sip,
                    src_addr: src,
                    dst_addr: dst,
                    payload: Bytes::from(payload),
                })
            })
            .collect();

        items.sort_by_key(|i| i.timestamp);
        Ok(items)
    }

    fn scan_rtp_packets(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> Result<Vec<(i32, u64, Vec<u8>)>> {
        let prefix = match leg_filter {
            Some(leg) => rtp_call_leg_prefix(call_id, leg),
            None => rtp_call_prefix(call_id),
        };

        let iter = self
            .engine
            .scan_prefix_time_range(&prefix, start_ts, end_ts)?;

        let mut packets: Vec<(i32, u64, Vec<u8>)> = iter
            .filter_map(|result| {
                let rec = result.ok()?;
                let (leg, _src, payload) = decode_rtp_value(&rec.value).ok()?;
                Some((leg, rec.ts as u64, payload))
            })
            .collect();

        packets.sort_by_key(|p| p.1);
        Ok(packets)
    }

    fn scan_media_sources(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> Result<HashMap<i32, Vec<String>>> {
        let prefix = match leg_filter {
            Some(leg) => rtp_call_leg_prefix(call_id, leg),
            None => rtp_call_prefix(call_id),
        };

        let iter = self
            .engine
            .scan_prefix_time_range(&prefix, start_ts, end_ts)?;

        let mut leg_sources: HashMap<i32, Vec<String>> = HashMap::new();
        let mut seen: std::collections::HashSet<(i32, String)> = std::collections::HashSet::new();

        for result in iter {
            let Ok(rec) = result else { continue };
            let Ok((leg, src, _payload)) = decode_rtp_value(&rec.value) else {
                continue;
            };
            if seen.insert((leg, src.clone())) {
                leg_sources.entry(leg).or_default().push(src);
            }
        }

        Ok(leg_sources)
    }

    fn build_payload_maps(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> (PayloadTypeMap, LegPayloadTypeMap) {
        let leg_sources = self
            .scan_media_sources(call_id, start_ts, end_ts, leg_filter)
            .unwrap_or_default();
        let flow = self.scan_sip_flow_in_range(start_ts, end_ts).unwrap_or_default();
        let payload_map = build_payload_type_map(&flow);
        let leg_payload_map = build_payload_type_map_by_leg(&flow, &leg_sources);
        (payload_map, leg_payload_map)
    }

    fn generate_wav(
        &self,
        call_id: &str,
        start_ts: i64,
        end_ts: i64,
        leg_filter: Option<i32>,
    ) -> Result<Vec<u8>> {
        let packets = self.scan_rtp_packets(call_id, start_ts, end_ts, leg_filter)?;
        if packets.is_empty() {
            return Ok(Vec::new());
        }
        let (payload_map, leg_payload_map) =
            self.build_payload_maps(call_id, start_ts, end_ts, leg_filter);
        let mut cursor = std::io::Cursor::new(Vec::new());
        generate_wav_to_writer(&packets, &payload_map, &leg_payload_map, true, &mut cursor)?;
        Ok(cursor.into_inner())
    }
}

#[async_trait]
impl SipFlowBackend for FlowDbBackend {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()> {
        if call_id.is_empty() {
            return Ok(());
        }

        let ts = item.timestamp as i64;
        let expire_at = self.make_expire_at(ts);
        let counter = self.next_counter();

        let record = match item.msg_type {
            SipFlowMsgType::Sip => {
                let key = make_sip_key(call_id, counter);
                let value = encode_sip_value(&item.src_addr, &item.dst_addr, &item.payload);
                Record { key, ts, expire_at, value }
            }
            SipFlowMsgType::Rtp => {
                let leg = item.leg.unwrap_or(0);
                let key = make_rtp_key(call_id, leg, counter);
                let value = encode_rtp_value(leg, &item.src_addr, &item.payload);
                Record { key, ts, expire_at, value }
            }
        };

        self.engine.write_batch_sync(vec![record])?;
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        self.engine.flush().await.map_err(|e| anyhow::anyhow!(e))
    }

    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        let prefix = sip_call_prefix(call_id);

        let iter = self.engine.scan_prefix_time_range(&prefix, start_ts, end_ts)?;

        let mut items: Vec<SipFlowItem> = iter
            .filter_map(|result| {
                let rec = result.ok()?;
                let (src, dst, payload) = decode_sip_value(&rec.value).ok()?;
                Some(SipFlowItem {
                    timestamp: rec.ts as u64,
                    seq: 0,
                    leg: None,
                    msg_type: SipFlowMsgType::Sip,
                    src_addr: src,
                    dst_addr: dst,
                    payload: Bytes::from(payload),
                })
            })
            .collect();

        items.sort_by_key(|i| i.timestamp);
        Ok(items)
    }

    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowMediaStats>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        let prefix = rtp_call_prefix(call_id);

        let iter = self
            .engine
            .scan_prefix_time_range(&prefix, start_ts, end_ts)?;

        let mut accumulators: HashMap<(i32, String, Option<u32>), MediaStatsAccumulator> =
            HashMap::new();

        for result in iter {
            let Ok(rec) = result else { continue };
            let Ok((leg, src, payload)) = decode_rtp_value(&rec.value) else {
                continue;
            };
            let header = parse_rtp_stats_header(&payload);
            let ssrc = header.map(|h| h.ssrc);
            let key = (leg, src.clone(), ssrc);

            accumulators
                .entry(key)
                .or_insert_with(|| MediaStatsAccumulator::new(leg, src, ssrc))
                .observe(rec.ts as u64, header);
        }

        Ok(accumulators
            .into_iter()
            .map(|(_, acc)| acc.into_stats())
            .collect())
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        self.generate_wav(call_id, start_ts, end_ts, None)
    }

    async fn query_media_stream(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<Vec<u8>> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();
        self.generate_wav(call_id, start_ts, end_ts, stream_leg)
    }

    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        let start_ts = start_time.timestamp_micros();
        let end_ts = end_time.timestamp_micros();

        let packets = self.scan_rtp_packets(call_id, start_ts, end_ts, stream_leg)?;
        if packets.is_empty() {
            return Err(anyhow::anyhow!("No media packets found"));
        }

        let (payload_map, leg_payload_map) =
            self.build_payload_maps(call_id, start_ts, end_ts, stream_leg);

        let mut file = tempfile::NamedTempFile::new()?;
        generate_wav_to_writer(&packets, &payload_map, &leg_payload_map, true, &mut file)?;
        std::io::Write::flush(&mut file)?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local_dt_from_micros(ts: i64) -> DateTime<Local> {
        Local
            .timestamp_micros(ts)
            .single()
            .expect("valid local datetime")
    }

    fn make_sip_item(ts_micros: u64, call_id: &str) -> SipFlowItem {
        let payload = format!(
            "INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {}\r\n",
            call_id
        );
        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: None,
            msg_type: SipFlowMsgType::Sip,
            src_addr: "127.0.0.1:5060".to_string(),
            dst_addr: "127.0.0.2:5060".to_string(),
            payload: Bytes::from(payload),
        }
    }

    fn make_rtp_item(ts_micros: u64, leg: i32, src: &str) -> SipFlowItem {
        let mut payload = vec![0x80u8, 0x00]; // RTP v2, PCMU
        payload.extend_from_slice(&1000u16.to_be_bytes()); // sequence
        payload.extend_from_slice(&160u32.to_be_bytes()); // timestamp
        payload.extend_from_slice(&1u32.to_be_bytes()); // ssrc
        payload.extend_from_slice(&vec![0x7Fu8; 160]); // PCMU payload

        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: Some(leg),
            msg_type: SipFlowMsgType::Rtp,
            src_addr: src.to_string(),
            dst_addr: String::new(),
            payload: Bytes::from(payload),
        }
    }

    fn make_rtp_item_with_seq(ts_micros: u64, leg: i32, src: &str, seq: u16) -> SipFlowItem {
        let mut payload = vec![0x80u8, 0x00]; // RTP v2, PCMU
        payload.extend_from_slice(&seq.to_be_bytes()); // sequence
        payload.extend_from_slice(&160u32.to_be_bytes()); // timestamp
        payload.extend_from_slice(&1u32.to_be_bytes()); // ssrc
        payload.extend_from_slice(&vec![0x7Fu8; 160]); // PCMU payload

        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: Some(leg),
            msg_type: SipFlowMsgType::Rtp,
            src_addr: src.to_string(),
            dst_addr: String::new(),
            payload: Bytes::from(payload),
        }
    }

    #[tokio::test]
    async fn test_flowdb_record_and_query_flow() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let call_id = "test-flow-1";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 1_000) as u64;
        let t1 = (base + 2_000) as u64;
        let t2 = (base + 3_000) as u64;

        backend
            .record(call_id, make_sip_item(t0, call_id))
            .unwrap();
        backend
            .record(call_id, make_sip_item(t1, call_id))
            .unwrap();
        backend
            .record(call_id, make_sip_item(t2, call_id))
            .unwrap();
        backend.flush().await.unwrap();

        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(base),
                local_dt_from_micros(base + 10_000),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].timestamp, t0);
        assert_eq!(items[1].timestamp, t1);
        assert_eq!(items[2].timestamp, t2);
        assert!(items[0].payload.starts_with(b"INVITE"));
    }

    #[tokio::test]
    async fn test_flowdb_query_flow_time_range() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let call_id = "test-flow-range";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 1_000) as u64;
        let t1 = (base + 2_000) as u64;
        let t2 = (base + 3_000) as u64;

        backend
            .record(call_id, make_sip_item(t0, call_id))
            .unwrap();
        backend
            .record(call_id, make_sip_item(t1, call_id))
            .unwrap();
        backend
            .record(call_id, make_sip_item(t2, call_id))
            .unwrap();
        backend.flush().await.unwrap();

        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(t1 as i64),
                local_dt_from_micros(t1 as i64),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].timestamp, t1);
    }

    #[tokio::test]
    async fn test_flowdb_record_rtp_and_query_media() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let call_id = "test-rtp-1";
        let base = chrono::Utc::now().timestamp_micros();

        for i in 0..5u64 {
            let ts = (base + i as i64 * 20_000) as u64;
            backend
                .record(call_id, make_rtp_item(ts, 0, "127.0.0.1:4000"))
                .unwrap();
        }
        backend.flush().await.unwrap();

        let wav = backend
            .query_media(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 200_000),
            )
            .await
            .unwrap();

        assert!(!wav.is_empty());
        assert_eq!(&wav[..4], b"RIFF");
    }

    #[tokio::test]
    async fn test_flowdb_query_media_stats() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let call_id = "test-stats-1";
        let base = chrono::Utc::now().timestamp_micros();

        let mut seq = 1000u16;
        for i in 0..10u64 {
            let ts = (base + i as i64 * 20_000) as u64;
            let item = make_rtp_item_with_seq(ts, 0, "127.0.0.1:4000", seq);
            seq = seq.wrapping_add(1);
            backend.record(call_id, item).unwrap();
        }
        backend.flush().await.unwrap();

        let stats = backend
            .query_media_stats(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 500_000),
            )
            .await
            .unwrap();

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].leg, 0);
        assert_eq!(stats[0].packet_count, 10);
        assert_eq!(stats[0].lost_packets, 0);
    }

    #[tokio::test]
    async fn test_flowdb_query_media_stats_with_loss() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let call_id = "test-stats-loss";
        let base = chrono::Utc::now().timestamp_micros();

        // Write packets with a gap: seq 1000, then 1002 (skip 1001)
        for (i, seq) in [1000u16, 1002u16].iter().enumerate() {
            let ts = (base + i as i64 * 20_000) as u64;
            let item = make_rtp_item_with_seq(ts, 0, "127.0.0.1:4000", *seq);
            backend.record(call_id, item).unwrap();
        }
        backend.flush().await.unwrap();

        let stats = backend
            .query_media_stats(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 500_000),
            )
            .await
            .unwrap();

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].packet_count, 2);
        assert_eq!(stats[0].lost_packets, 1);
        assert_eq!(stats[0].expected_packets, 3);
    }

    #[tokio::test]
    async fn test_flowdb_media_stream_leg_filter() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let call_id = "test-leg-filter";
        let base = chrono::Utc::now().timestamp_micros();

        // Write to leg 0 and leg 1
        for i in 0..3u64 {
            let ts = (base + i as i64 * 20_000) as u64;
            backend
                .record(call_id, make_rtp_item(ts, 0, "127.0.0.1:4000"))
                .unwrap();
            backend
                .record(call_id, make_rtp_item(ts, 1, "127.0.0.1:4002"))
                .unwrap();
        }
        backend.flush().await.unwrap();

        // Query only leg 1
        let wav = backend
            .query_media_stream(
                call_id,
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 200_000),
                Some(1),
            )
            .await
            .unwrap();

        assert!(!wav.is_empty());
        assert_eq!(&wav[..4], b"RIFF");
    }

    #[tokio::test]
    async fn test_flowdb_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let base = chrono::Utc::now().timestamp_micros();

        let items = backend
            .query_flow(
                "nonexistent",
                local_dt_from_micros(base),
                local_dt_from_micros(base + 100_000),
            )
            .await
            .unwrap();

        assert!(items.is_empty());

        let wav = backend
            .query_media(
                "nonexistent",
                local_dt_from_micros(base),
                local_dt_from_micros(base + 100_000),
            )
            .await
            .unwrap();

        assert!(wav.is_empty());
    }

    #[tokio::test]
    async fn test_flowdb_isolation_between_calls() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        let base = chrono::Utc::now().timestamp_micros();

        backend
            .record("call-a", make_sip_item(base as u64, "call-a"))
            .unwrap();
        backend
            .record("call-b", make_sip_item(base as u64, "call-b"))
            .unwrap();
        backend.flush().await.unwrap();

        let items_a = backend
            .query_flow(
                "call-a",
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 1),
            )
            .await
            .unwrap();
        let items_b = backend
            .query_flow(
                "call-b",
                local_dt_from_micros(base - 1),
                local_dt_from_micros(base + 1),
            )
            .await
            .unwrap();

        assert_eq!(items_a.len(), 1);
        assert_eq!(items_b.len(), 1);
        assert!(String::from_utf8_lossy(&items_a[0].payload).contains("call-a"));
        assert!(String::from_utf8_lossy(&items_b[0].payload).contains("call-b"));
    }

    #[tokio::test]
    async fn test_flowdb_flush_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();
        backend.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_flowdb_recovery_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let call_id = "test-recovery";
        let base = chrono::Utc::now().timestamp_micros();
        let path = dir.path().to_path_buf();

        {
            let backend = FlowDbBackend::new(&path, None, 1, 16).unwrap();
            backend
                .record(call_id, make_sip_item(base as u64, call_id))
                .unwrap();
            backend.flush().await.unwrap();
        }

        {
            let backend = FlowDbBackend::new(&path, None, 1, 16).unwrap();
            let items = backend
                .query_flow(
                    call_id,
                    local_dt_from_micros(base - 1),
                    local_dt_from_micros(base + 1),
                )
                .await
                .unwrap();
            assert_eq!(items.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_flowdb_skip_empty_call_id() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FlowDbBackend::new(dir.path(), None, 1, 16).unwrap();

        // Empty call_id should be silently skipped
        backend.record("", make_sip_item(1000, "")).unwrap();
        backend.flush().await.unwrap();
    }
}
