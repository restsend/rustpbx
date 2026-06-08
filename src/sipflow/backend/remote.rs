use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::SipFlowClusterNode;
use crate::http_util::{HttpFetchOptions, fetch_bytes, fetch_json};
use crate::sipflow::backend::SipFlowBackend;
use crate::sipflow::protocol::{MsgType, Packet, encode_packet};
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

enum Command {
    RecordItem { call_id: String, item: SipFlowItem },
}

/// Remote backend that sends data to one of several remote sipflow servers
/// via UDP (write) and HTTP (read). The target node is selected by
/// consistent hashing on the call_id.
pub struct RemoteBackend {
    sender: mpsc::UnboundedSender<Command>,
    nodes: Vec<ClusterNode>,
    client: reqwest::Client,
    cancel_token: CancellationToken,
}

impl RemoteBackend {
    pub fn new(config_nodes: Vec<SipFlowClusterNode>, timeout_secs: u64) -> Result<Self> {
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

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
        let cancel_token = CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();
        let nodes_clone = nodes.clone();

        crate::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token_clone.cancelled() => {
                        break;
                    }
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            Command::RecordItem { call_id, item } => {
                                let idx = jump_consistent_hash(&call_id, nodes_clone.len());
                                let target_addr = nodes_clone[idx].udp_addr;

                                let default_port = if matches!(&item.msg_type, SipFlowMsgType::Sip)
                                {
                                    5060
                                } else {
                                    0
                                };
                                let parse_addr = |s: &str| -> (IpAddr, u16) {
                                    let parts: Vec<&str> = s.split(':').collect();
                                    let ip = parts[0].parse().unwrap_or(IpAddr::from([127, 0, 0, 1]));
                                    let port = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(default_port);
                                    (ip, port)
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
                                let (packet_call_id, packet_leg) = if msg_type == MsgType::Rtp {
                                    (Some(call_id), item.leg)
                                } else {
                                    (None, None)
                                };

                                let packet = Packet {
                                    msg_type,
                                    src: (src_ip, src_port),
                                    dst: (dst_ip, dst_port),
                                    timestamp: item.timestamp,
                                    call_id: packet_call_id,
                                    leg: packet_leg,
                                    payload: item.payload,
                                };

                                let data = encode_packet(&packet);
                                let _ = udp_socket.send_to(&data, target_addr).await;
                            }
                        }
                    }
                }
            }
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

#[async_trait]
impl SipFlowBackend for RemoteBackend {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()> {
        self.sender.send(Command::RecordItem {
            call_id: call_id.to_string(),
            item,
        })?;
        Ok(())
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

        let json: serde_json::Value = fetch_json(&self.client, &url, &HttpFetchOptions::new()).await?;

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

        let json: serde_json::Value = fetch_json(&self.client, &url, &HttpFetchOptions::new()).await?;

        if json["status"] == "success" {
            let stats = json["stats"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let packet_count = v
                                .get("packet_count")
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
