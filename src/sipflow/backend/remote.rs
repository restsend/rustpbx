use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::config::SipFlowClusterNode;
use crate::http_util::{HttpFetchOptions, fetch_bytes, fetch_json};
use crate::sipflow::backend::SipFlowBackend;
use crate::sipflow::protocol::{
    MAX_BATCH_COUNT, MsgType, Packet, encode_batch_into, encode_packet_into,
};
use crate::sipflow::{SipFlowItem, SipFlowMediaStats, SipFlowMsgType};

/// Jump Consistent Hash
///
/// Maps a key to a bucket in `[0, num_buckets)` with near-perfect uniformity.
/// - O(1) space, O(log n) time
/// - Adding/removing a bucket shifts only 1/n of the keys
/// - Deterministic: same key → same bucket
///
/// Reference: https://arxiv.org/abs/1406.2294
pub fn jump_consistent_hash(key: &str, num_buckets: usize) -> usize {
    if num_buckets == 1 {
        return 0;
    }
    let mut hash: u64 = 0;
    for b in key.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as u64);
    }
    let mut b: i64 = -1;
    let mut j: i64 = 0;
    while j < num_buckets as i64 {
        b = j;
        hash = hash.wrapping_mul(2862933555777941757).wrapping_add(1);
        let shift = hash >> 33;
        j = (((b as i64 + 1) as f64) * ((1u64 << 31) as f64) / ((shift as u64 + 1) as f64)) as i64;
    }
    b as usize
}

#[derive(Clone)]
struct ClusterNode {
    udp_addr: SocketAddr,
    http_addr: String,
}

/// Minimum (and default) flush interval. Values below this in config are
/// silently raised to it, because flushing more often than once per RTP
/// frame interval (~20ms) defeats the purpose of batching and risks
/// tight-loop sending under low load.
const MIN_BATCH_FLUSH_MS: u64 = 20;

/// Default ingest channel capacity when the config value is 0.
const DEFAULT_CHANNEL_CAPACITY: usize = 8192;

/// Lower bound on channel capacity. A capacity of 0 would make `record()`
/// always fail, so any sub-1 value is raised to this.
const MIN_CHANNEL_CAPACITY: usize = 1;

enum Command {
    RecordItem { call_id: String, item: SipFlowItem },
}

/// Remote backend that sends data to one of several remote sipflow servers
/// via UDP (write) and HTTP (read). The target node is selected by
/// consistent hashing on the call_id.
///
/// Writes are coalesced per destination node: items are accumulated into a
/// per-node buffer and sent as a single batched UDP datagram when either
/// `batch_size` packets are pending or `batch_flush_ms` elapses since the
/// last flush. This amortizes syscall and allocation cost across many
/// packets, dramatically reducing CPU under load compared to a
/// one-datagram-per-item sender.
///
/// Setting `batch_size = 0` disables batching entirely: each record is
/// sent immediately as a single-packet (legacy format) UDP datagram. This
/// is the safe escape hatch if a receiver is too old to understand the
/// batch wire format.
pub struct RemoteBackend {
    sender: mpsc::Sender<Command>,
    nodes: Vec<ClusterNode>,
    client: reqwest::Client,
    cancel_token: CancellationToken,
}

impl RemoteBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_nodes: Vec<SipFlowClusterNode>,
        timeout_secs: u64,
        batch_size_cfg: usize,
        batch_flush_ms_cfg: u64,
        channel_capacity_cfg: usize,
    ) -> Result<Self> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        let udp_socket = Arc::new(UdpSocket::from_std(socket)?);

        let nodes: Vec<ClusterNode> = config_nodes
            .iter()
            .map(|n| {
                let udp_addr: SocketAddr = n.udp.to_socket_addrs()?.next().ok_or_else(|| {
                    anyhow::anyhow!("Unable to resolve SipFlow UDP address: {}", n.udp)
                })?;
                Ok(ClusterNode {
                    udp_addr,
                    http_addr: n.http.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let client = crate::http_util::build_keepalive_client(
            Some(std::time::Duration::from_secs(timeout_secs)),
            None,
        )?;

        // Ingest channel capacity: 0 → default, otherwise clamp to >= 1.
        let channel_capacity = if channel_capacity_cfg == 0 {
            DEFAULT_CHANNEL_CAPACITY
        } else {
            channel_capacity_cfg.max(MIN_CHANNEL_CAPACITY)
        };
        let (tx, rx) = mpsc::channel::<Command>(channel_capacity);
        let cancel_token = CancellationToken::new();

        // `batch_size = 0` disables batching entirely (each record is sent
        // as a legacy single-packet datagram). Otherwise clamp to a sane
        // range. We keep a separate `batch_enabled` flag so the flush path
        // can choose the right wire format (single vs batch).
        //
        // `clamp(1, MAX_BATCH_COUNT)` maps 0 → 1 (so the per-node push/flush
        // logic still works when disabled) and caps pathological large
        // values at the protocol maximum.
        let batch_enabled = batch_size_cfg != 0;
        let batch_size = batch_size_cfg.clamp(1, MAX_BATCH_COUNT);
        // Clamp flush interval to at least MIN_BATCH_FLUSH_MS. Anything
        // tighter would defeat batching and risk spin-flushing under low
        // load.
        let flush_duration =
            Duration::from_millis(batch_flush_ms_cfg.max(MIN_BATCH_FLUSH_MS));

        let cancel_clone = cancel_token.clone();
        let nodes_clone = nodes.clone();

        crate::utils::spawn(async move {
            worker_loop(
                rx,
                udp_socket,
                nodes_clone,
                batch_size,
                batch_enabled,
                flush_duration,
                cancel_clone,
            )
            .await;
        });

        Ok(Self {
            sender: tx,
            nodes,
            client,
            cancel_token,
        })
    }

    fn select_node(&self, call_id: &str) -> &ClusterNode {
        let idx = jump_consistent_hash(call_id, self.nodes.len());
        &self.nodes[idx]
    }
}

/// Build a wire [`Packet`] from a recorded item, consuming `call_id` to
/// avoid an extra clone on the RTP path (where the call id travels in the
/// packet metadata).
///
/// Address parsing uses `split_once` instead of `split(':').collect()` to
/// avoid heap allocations on every packet.
fn build_packet(call_id: String, item: SipFlowItem) -> Packet {
    let default_port = if matches!(item.msg_type, SipFlowMsgType::Sip) {
        5060
    } else {
        0
    };

    let parse_addr = |s: &str| -> (IpAddr, u16) {
        match s.split_once(':') {
            Some((ip_str, port_str)) => {
                let ip = ip_str.parse().unwrap_or(IpAddr::from([127, 0, 0, 1]));
                let port = port_str.parse().unwrap_or(default_port);
                (ip, port)
            }
            None => {
                let ip = s.parse().unwrap_or(IpAddr::from([127, 0, 0, 1]));
                (ip, default_port)
            }
        }
    };

    let (src_ip, src_port) = if !item.src_addr.is_empty() {
        parse_addr(&item.src_addr)
    } else {
        (IpAddr::from([127, 0, 0, 1]), default_port)
    };
    let (dst_ip, dst_port) = if !item.dst_addr.is_empty() {
        parse_addr(&item.dst_addr)
    } else {
        (IpAddr::from([127, 0, 0, 1]), default_port)
    };

    let msg_type = match item.msg_type {
        SipFlowMsgType::Sip => MsgType::Sip,
        SipFlowMsgType::Rtp => MsgType::Rtp,
    };
    // For RTP the call_id is embedded in the packet metadata; for SIP it is
    // recovered from the payload on the receiver side (`extract_callid`),
    // so we move `call_id` only for RTP and let it drop for SIP.
    let (packet_call_id, packet_leg) = if msg_type == MsgType::Rtp {
        (Some(call_id), item.leg)
    } else {
        (None, None)
    };

    Packet {
        msg_type,
        src: (src_ip, src_port),
        dst: (dst_ip, dst_port),
        timestamp: item.timestamp,
        call_id: packet_call_id,
        leg: packet_leg,
        payload: item.payload,
    }
}

/// Background worker that drains the ingest channel, groups packets by
/// destination node, and flushes each node's buffer.
///
/// When `batch_enabled` is true, each flush encodes the node's pending
/// packets as a single batched UDP datagram (one syscall for many
/// packets). When false (config `batch_size = 0`), each packet is sent
/// immediately as a legacy single-packet datagram — the worker still
/// threads everything through the same per-node buffers for uniformity,
/// but the buffers never accumulate (every push triggers a flush).
///
/// Flush triggers:
///   1. A node's buffer reaches `batch_size` → immediate flush of that node.
///   2. `flush_duration` elapses since the last periodic flush → flush all
///      non-empty buffers (bounds latency under low load).
///   3. Channel closed or cancellation → final flush of all buffers.
async fn worker_loop(
    mut rx: mpsc::Receiver<Command>,
    udp_socket: Arc<UdpSocket>,
    nodes: Vec<ClusterNode>,
    batch_size: usize,
    batch_enabled: bool,
    flush_duration: Duration,
    cancel: CancellationToken,
) {
    // One pending-packet buffer per destination node.
    let mut per_node: Vec<Vec<Packet>> = (0..nodes.len()).map(|_| Vec::new()).collect();
    // Scratch buffer reused across `recv_many` calls to avoid allocation.
    // Sized to `batch_size` so a single call can pull up to one full batch.
    let mut scratch: Vec<Command> = Vec::with_capacity(batch_size);
    // Reusable wire-encoding buffer, kept across flushes for zero-alloc sends.
    let mut send_buf: Vec<u8> = Vec::new();

    let mut deadline = Instant::now() + flush_duration;
    // Cache the recv limit outside the select! arm to avoid simultaneous
    // mutable+immutable borrows of `scratch`.
    let recv_limit = scratch.capacity().max(1);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                flush_all(&mut per_node, &udp_socket, &nodes, &mut send_buf, batch_enabled).await;
                break;
            }
            // `recv_many` returns 0 only when the channel is closed.
            n = rx.recv_many(&mut scratch, recv_limit) => {
                if n == 0 {
                    flush_all(&mut per_node, &udp_socket, &nodes, &mut send_buf, batch_enabled).await;
                    break;
                }
                for cmd in scratch.drain(..) {
                    let Command::RecordItem { call_id, item } = cmd;
                    let idx = jump_consistent_hash(&call_id, nodes.len());
                    let packet = build_packet(call_id, item);
                    let node_buf = &mut per_node[idx];
                    node_buf.push(packet);
                    if node_buf.len() >= batch_size {
                        flush_one(node_buf, &udp_socket, nodes[idx].udp_addr, &mut send_buf, batch_enabled).await;
                    }
                }
            }
            // Periodic flush: bounds latency to ~flush_duration for low-rate
            // streams (e.g. trickled RTP at 50pps). With batching disabled
            // this is effectively a no-op (buffers are always empty).
            _ = tokio::time::sleep_until(deadline) => {
                flush_all(&mut per_node, &udp_socket, &nodes, &mut send_buf, batch_enabled).await;
                deadline = Instant::now() + flush_duration;
            }
        }
    }
}

/// Encode and send a single node's pending buffer.
///
/// When `batch_enabled` is true the buffer is sent as one batched datagram.
/// When false each packet is sent as its own legacy single-packet datagram
/// (used when the user sets `batch_size = 0` to opt out of batching).
///
/// `send_buf` is cleared and reused across calls to avoid per-flush
/// allocation. The packet buffer is cleared after a successful send.
async fn flush_one(
    buf: &mut Vec<Packet>,
    udp_socket: &UdpSocket,
    target_addr: SocketAddr,
    send_buf: &mut Vec<u8>,
    batch_enabled: bool,
) {
    if buf.is_empty() {
        return;
    }
    if batch_enabled {
        send_buf.clear();
        if encode_batch_into(send_buf, buf).is_ok() {
            let _ = udp_socket.send_to(send_buf, target_addr).await;
        } else {
            // Encoding can only fail on >MAX_BATCH_COUNT, which we prevent
            // via clamping in `new()`. Fall back to per-packet sends
            // defensively.
            for packet in buf.iter() {
                send_buf.clear();
                encode_packet_into(send_buf, packet);
                let _ = udp_socket.send_to(send_buf, target_addr).await;
            }
        }
    } else {
        // Batching disabled (`batch_size = 0`): legacy single-packet
        // datagrams. Buffer should normally contain exactly 1 packet here
        // (every push triggers a flush), but loop anyway for safety.
        for packet in buf.iter() {
            send_buf.clear();
            encode_packet_into(send_buf, packet);
            let _ = udp_socket.send_to(send_buf, target_addr).await;
        }
    }
    buf.clear();
}

/// Flush every node's pending buffer. Iterates in node order so the same
/// `send_buf` can be reused safely (each `flush_one` completes before the
/// next begins).
async fn flush_all(
    per_node: &mut [Vec<Packet>],
    udp_socket: &UdpSocket,
    nodes: &[ClusterNode],
    send_buf: &mut Vec<u8>,
    batch_enabled: bool,
) {
    for (i, node) in nodes.iter().enumerate() {
        flush_one(
            &mut per_node[i],
            udp_socket,
            node.udp_addr,
            send_buf,
            batch_enabled,
        )
        .await;
    }
}

#[async_trait]
impl SipFlowBackend for RemoteBackend {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()> {
        // `try_send` (not `send().await`) because this is a sync function:
        // never blocks, never suspends the caller. On a full bounded channel
        // (sustained overload) this returns `TrySendError::Full`, which
        // upstream callers already swallow — preferable to unbounded memory
        // growth under backpressure.
        self.sender
            .try_send(Command::RecordItem {
                call_id: call_id.to_string(),
                item,
            })
            .map_err(anyhow::Error::from)
    }

    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let node = self.select_node(call_id);
        let url = format!(
            "{}/flow?callid={}&start={}&end={}",
            node.http_addr,
            call_id,
            start_time.timestamp(),
            end_time.timestamp()
        );

        let json: serde_json::Value =
            fetch_json(&self.client, &url, &HttpFetchOptions::new()).await?;

        if json["status"] == "success" {
            let flow_array = json["flow"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid response format: flow is not an array"))?;

            let mut items: Vec<SipFlowItem> = flow_array
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect();

            items.sort_by_key(|i| i.timestamp);

            Ok(items)
        } else {
            Err(anyhow::anyhow!(
                "Query failed: {}",
                json["message"].as_str().unwrap_or("Unknown error")
            ))
        }
    }

    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowMediaStats>> {
        let node = self.select_node(call_id);
        let url = format!(
            "{}/media?callid={}&start={}&end={}&stats=1",
            node.http_addr,
            call_id,
            start_time.timestamp(),
            end_time.timestamp()
        );

        let json: serde_json::Value =
            fetch_json(&self.client, &url, &HttpFetchOptions::new()).await?;

        if json["status"] == "success" {
            let stats = json["stats"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let packet_count =
                                v.get("packet_count")
                                    .or_else(|| v.get("count"))
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0) as usize;
                            let lost_packets = v
                                .get("lost_packets")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                            let expected_packets = v
                                .get("expected_packets")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(packet_count as u64 + lost_packets);

                            Some(SipFlowMediaStats {
                                leg: v.get("leg")?.as_i64()? as i32,
                                src: v.get("src")?.as_str()?.to_string(),
                                packet_count,
                                lost_packets,
                                expected_packets,
                                loss_percent: v
                                    .get("loss_percent")
                                    .and_then(|value| value.as_f64())
                                    .unwrap_or_else(|| {
                                        if expected_packets > 0 {
                                            lost_packets as f64 / expected_packets as f64 * 100.0
                                        } else {
                                            0.0
                                        }
                                    }),
                                jitter_ms: v.get("jitter_ms").and_then(|value| value.as_f64()),
                                ssrc: v
                                    .get("ssrc")
                                    .and_then(|value| value.as_u64())
                                    .map(|value| value as u32),
                                payload_type: v
                                    .get("payload_type")
                                    .and_then(|value| value.as_u64())
                                    .map(|value| value as u8),
                                clock_rate: v
                                    .get("clock_rate")
                                    .and_then(|value| value.as_u64())
                                    .map(|value| value as u32),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(stats)
        } else {
            Err(anyhow::anyhow!(
                "Media stats query failed: {}",
                json["message"].as_str().unwrap_or("Unknown error")
            ))
        }
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        let node = self.select_node(call_id);
        let url = format!(
            "{}/media?callid={}&start={}&end={}&format=pcm",
            node.http_addr,
            call_id,
            start_time.timestamp(),
            end_time.timestamp()
        );

        let bytes = fetch_bytes(
            &self.client,
            reqwest::Method::GET,
            &url,
            &HttpFetchOptions::new(),
        )
        .await?;
        Ok(bytes.to_vec())
    }

    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        _stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        let node = self.select_node(call_id);
        let url = format!(
            "{}/media?callid={}&start={}&end={}",
            node.http_addr,
            call_id,
            start_time.timestamp(),
            end_time.timestamp()
        );

        let file = tokio::task::spawn_blocking(|| tempfile::NamedTempFile::new())
            .await
            .map_err(|e| anyhow::anyhow!("temp file creation failed: {e}"))??;

        let std_file = file.reopen()?;
        let mut tokio_file = tokio::fs::File::from_std(std_file);
        crate::http_util::fetch_to_writer(
            &self.client,
            reqwest::Method::GET,
            &url,
            &HttpFetchOptions::new(),
            &mut tokio_file,
        )
        .await?;

        Ok(file)
    }
}

impl Drop for RemoteBackend {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jump_hash_single_bucket() {
        for key in ["", "a", "hello", "call-id-12345"] {
            assert_eq!(jump_consistent_hash(key, 1), 0);
        }
    }

    #[test]
    fn test_jump_hash_deterministic() {
        let keys = [
            "",
            "a",
            "hello",
            "call-id-12345",
            "very-long-call-id-that-exceeds-64-chars-in-length-abcdefghijklmnopqrstuvwxyz",
        ];
        for key in &keys {
            let h1 = jump_consistent_hash(key, 10);
            let h2 = jump_consistent_hash(key, 10);
            assert_eq!(h1, h2, "hash must be deterministic for key: {key}");
        }
    }

    #[test]
    fn test_jump_hash_different_keys() {
        let keys = ["call-a", "call-b", "call-c", "call-d", "call-e"];
        let results: std::collections::HashSet<usize> =
            keys.iter().map(|k| jump_consistent_hash(k, 4)).collect();
        // At least 2 different buckets out of 5 keys and 4 buckets (high probability)
        assert!(results.len() >= 2, "expected at least 2 distinct buckets");
    }

    #[test]
    fn test_jump_hash_distribution() {
        let num_buckets = 4;
        let num_keys = 10000;
        let mut counts = vec![0usize; num_buckets];

        for i in 0..num_keys {
            let key = format!("call-id-{:020}", i);
            let bucket = jump_consistent_hash(&key, num_buckets);
            counts[bucket] += 1;
        }

        let expected = num_keys / num_buckets;
        let tolerance = (expected as f64 * 0.10) as usize; // ±10%
        for (i, &count) in counts.iter().enumerate() {
            assert!(
                count.abs_diff(expected) <= tolerance,
                "bucket {i} has {count} keys, expected ~{expected} (±{tolerance})"
            );
        }
    }

    #[test]
    fn test_jump_hash_minimal_disruption() {
        // When adding a new bucket, only ~1/n keys should move.
        // Here 4→5 should move ~20% of keys.
        let num_keys = 10000;
        let mut moved = 0;

        for i in 0..num_keys {
            let key = format!("call-id-{:020}", i);
            let b4 = jump_consistent_hash(&key, 4);
            let b5 = jump_consistent_hash(&key, 5);
            if b4 != b5 {
                moved += 1;
            }
        }

        // With 4→5, ~20% of keys should move. Allow ±5% margin.
        let ratio = moved as f64 / num_keys as f64;
        assert!(
            (ratio - 0.2).abs() < 0.05,
            "expected ~20% keys to move from 4→5 buckets, got {:.1}%",
            ratio * 100.0
        );
    }
}
