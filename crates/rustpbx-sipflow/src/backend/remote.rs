use anyhow::Result;
use async_trait::async_trait;
use bytes::BufMut;
use chrono::{DateTime, Local};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;
use tokio::net::lookup_host;
use tokio_util::sync::CancellationToken;

use tracing::{info, warn};

use std::borrow::Cow;
use crate::backend::SipFlowBackend;
use crate::config::SipFlowClusterNode;
use crate::perf::PerfCounters;
use crate::protocol::{
    BATCH_MAGIC, BATCH_VERSION, MsgType, Packet, encode_batch_into, encode_packet_into,
};
use crate::{SipFlowItem, SipFlowMediaStats, SipFlowMsgType};
use arc_swap::ArcSwap;
use rustpbx_http_util::{HttpFetchOptions, fetch_bytes, fetch_json};

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
struct RemoteNode {
    udp_host: String,
    http_addr: String,
    udp_addr: Arc<ArcSwap<SocketAddr>>,
}

/// Default ingest channel capacity when the config value is 0.
const DEFAULT_CHANNEL_CAPACITY: usize = 8192;

/// Lower bound on channel capacity. A capacity of 0 would make `record()`
/// always fail, so any sub-1 value is raised to this.
const MIN_CHANNEL_CAPACITY: usize = 1;

/// Per-node channel capacity as fraction of total ingress capacity.
enum Command {
    RecordItem { call_id: String, item: SipFlowItem },
}

/// Remote backend that sends data to one of several remote sipflow servers
/// via UDP (write) and HTTP (read). The target node is selected by
/// consistent hashing on the call_id.
///
/// ## Architecture
///
/// ```text
/// record() callers
///   │ try_send()
///   ▼
/// std::sync::mpsc::SyncSender<Command> (bounded ingress)
///   │
///   ▼
/// [Dispatcher Thread] (1 OS thread) — pure routing, no accumulation
///   │ build_packet + jump_consistent_hash → per-node channel
///   │
///   ├── std::sync::mpsc::SyncSender<Packet> → [Sender Thread 0]
///   │     std::net::UdpSocket (independent)
///   │     Accumulates into Vec<Packet>
///   │     MTU-aware batch encoding → send_to()
///   │
///   ├── std::sync::mpsc::SyncSender<Packet> → [Sender Thread 1]
///   │     ...
///   └── ...
/// ```
///
/// Batching is driven by packet accumulation in each sender thread:
///   - When adding a packet would exceed the MTU budget, the pending batch
///     is flushed as a single batched UDP datagram.
///   - A periodic timeout (20ms) flushes partially-filled batches to bound
///     latency under low load.
///   - No compression is applied — the client side is kept CPU-light.
///
/// The receiver side supports both legacy single-packet datagrams and the
/// batched wire format transparently, so enabling batching here does not
/// require matching receiver changes.
///
/// ## DNS TTL
/// When `dns_ttl_secs > 0`, a background task periodically re-resolves
/// each node's `udp` hostname. If the resolved IP changes (e.g. due to
/// load-balancer rotation or failover), the new address is used for
/// subsequent sends without restarting the service.
pub struct RemoteBackend {
    sender: SyncSender<Command>,
    nodes: Vec<RemoteNode>,
    client: reqwest::Client,
    cancel_token: CancellationToken,
    perf: Arc<PerfCounters>,
    #[allow(dead_code)]
    client_id: u32,
    _dispatcher_handle: Option<std::thread::JoinHandle<()>>,
    _sender_handles: Vec<std::thread::JoinHandle<()>>,
}

impl RemoteBackend {
    pub async fn new(
        config_nodes: Vec<SipFlowClusterNode>,
        timeout_secs: u64,
        channel_capacity_cfg: usize,
        mtu: usize,
        dns_ttl_secs: u64,
        report_interval_secs: u64,
        cancel_token: CancellationToken,
    ) -> Result<Self> {
        let cancel_token = cancel_token.child_token();

        let mut nodes = Vec::with_capacity(config_nodes.len());
        for node in config_nodes {
            let udp_addr: SocketAddr =
                lookup_host(node.udp.as_str())
                    .await?
                    .next()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Unable to resolve SipFlow UDP address: {}", node.udp)
                    })?;
            nodes.push(RemoteNode {
                udp_host: node.udp,
                http_addr: node.http,
                udp_addr: Arc::new(ArcSwap::new(Arc::new(udp_addr))),
            });
        }

        let client = rustpbx_http_util::build_keepalive_client(
            Some(std::time::Duration::from_secs(timeout_secs)),
            None,
        )?;

        let client_id = loop {
            let id = rand::random::<u32>();
            if id != 0 {
                break id;
            }
        };

        // Ingest channel capacity: 0 → default, otherwise clamp to >= 1.
        let channel_capacity = if channel_capacity_cfg == 0 {
            DEFAULT_CHANNEL_CAPACITY
        } else {
            channel_capacity_cfg.max(MIN_CHANNEL_CAPACITY)
        };
        let (tx, rx) = mpsc::sync_channel::<Command>(channel_capacity);

        // Per-node channel capacity: distribute total capacity across nodes
        let node_count = nodes.len();
        let per_node_cap = (channel_capacity / node_count).max(64);
        let mut node_senders = Vec::with_capacity(node_count);

        let perf = PerfCounters::new_arc();
        let cancel_dispatcher = cancel_token.clone();
        let cancel_dns = cancel_token.clone();
        let cancel_report = cancel_token.clone();
        let perf_dispatcher = perf.clone();

        // Per-node data for report loop: (node, sent_count)
        let mut report_nodes: Vec<(RemoteNode, Arc<AtomicU64>)> = Vec::with_capacity(node_count);

        // Create per-node SyncSender channels and spawn sender threads
        let mut sender_handles = Vec::with_capacity(node_count);
        let cancel_sender = cancel_token.clone();
        for i in 0..node_count {
            let (node_tx, node_rx) = mpsc::sync_channel::<Packet>(per_node_cap);
            node_senders.push(node_tx);

            let sent_count = Arc::new(AtomicU64::new(0));
            report_nodes.push((nodes[i].clone(), sent_count.clone()));

            let node_addr = nodes[i].udp_addr.clone();
            let node_perf = perf.clone();
            let cancel = cancel_sender.clone();
            let handle = std::thread::Builder::new()
                .name(format!("sipflow-send-{i}"))
                .spawn(move || {
                    sender_thread(node_rx, node_addr, mtu, i, node_perf, sent_count, cancel);
                })?;
            sender_handles.push(handle);
        }

        // Spawn dispatcher thread
        let dispatcher_client_id = client_id;
        let dispatcher_handle = std::thread::Builder::new()
            .name("sipflow-dispatch".to_string())
            .spawn(move || {
                dispatcher_thread(
                    rx,
                    node_senders,
                    cancel_dispatcher,
                    perf_dispatcher,
                    dispatcher_client_id,
                );
            })?;

        // Start DNS refresh task if TTL is configured
        if dns_ttl_secs > 0 {
            let dns_nodes = nodes.clone();
            tokio::spawn(
                async move { dns_refresh_loop(dns_nodes, dns_ttl_secs, cancel_dns).await },
            );
        }

        // Start report loop if interval > 0
        if report_interval_secs > 0 {
            let report_client = client.clone();
            tokio::spawn(async move {
                report_loop(
                    report_nodes,
                    report_client,
                    report_interval_secs,
                    client_id,
                    cancel_report,
                )
                .await
            });
        }

        Ok(Self {
            sender: tx,
            nodes,
            client,
            cancel_token,
            perf,
            client_id,
            _dispatcher_handle: Some(dispatcher_handle),
            _sender_handles: sender_handles,
        })
    }

    fn select_node(&self, call_id: &str) -> &RemoteNode {
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
fn build_packet(call_id: String, item: SipFlowItem, client_id: u32) -> Packet {
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

    let is_synth = |s: &str| s.is_empty() || s == "synth";
    let (src_ip, src_port) = if !is_synth(&item.src_addr) {
        parse_addr(&item.src_addr)
    } else {
        (IpAddr::from([0, 0, 0, 0]), default_port)
    };
    let (dst_ip, dst_port) = if !is_synth(&item.dst_addr) {
        parse_addr(&item.dst_addr)
    } else {
        (IpAddr::from([0, 0, 0, 0]), default_port)
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

    Packet {
        msg_type,
        src: (src_ip, src_port),
        dst: (dst_ip, dst_port),
        timestamp: item.timestamp,
        call_id: packet_call_id,
        leg: packet_leg,
        payload: item.payload,
        client_id,
    }
}

/// Dispatcher thread: receives Commands from the ingress channel, converts
/// each to a Packet, routes to the correct per-node channel via consistent
/// hashing.  This is a pure routing layer — no accumulation, no batch
/// processing, no compression.  Packets that cannot be enqueued to a node
/// channel (channel full) are silently dropped and counted.
fn dispatcher_thread(
    rx: mpsc::Receiver<Command>,
    node_senders: Vec<SyncSender<Packet>>,
    cancel: CancellationToken,
    perf: Arc<PerfCounters>,
    client_id: u32,
) {
    let node_count = node_senders.len();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(cmd) => {
                let Command::RecordItem { call_id, item } = cmd;
                let is_signaling = matches!(item.msg_type, SipFlowMsgType::Sip);
                let packet = build_packet(call_id, item, client_id);
                let idx =
                    jump_consistent_hash(&packet.call_id.as_deref().unwrap_or(""), node_count);

                match node_senders[idx].try_send(packet) {
                    Ok(()) => {
                        if is_signaling {
                            perf.signaling_sent.fetch_add(1, Ordering::Relaxed);
                        } else {
                            perf.media_sent.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        perf.items_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Sender thread: receives Packets from the dispatcher, accumulates into a
/// Vec<Packet>, and flushes via MTU-aware batch encoding.
///
/// Flush triggers:
///   1. A packet is received and the current batch combined with it would
///      exceed the MTU — flush the current batch first, then start a new one.
///   2. No packet arrives within `FLUSH_DURATION` — flush any pending batch
///      to bound latency under low load.
///   3. Channel disconnected — final flush and exit.
///
/// No compression is applied.  Single-packet batches use the legacy
/// single-packet wire format; multi-packet batches use the batched format.
fn sender_thread(
    rx: mpsc::Receiver<Packet>,
    target_addr: Arc<ArcSwap<SocketAddr>>,
    mtu: usize,
    node_index: usize,
    _perf: Arc<PerfCounters>,
    sent_count: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    let socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[sender-{node_index}] failed to bind UDP socket: {e}");
            return;
        }
    };
    let _ = socket.set_write_timeout(Some(Duration::from_secs(5)));

    const FLUSH_DURATION: Duration = Duration::from_millis(20);
    let mut batch: Vec<Packet> = Vec::new();
    let initial_cap = if mtu > 0 { mtu } else { 65535 };
    let mut send_buf: Vec<u8> = Vec::with_capacity(initial_cap);

    let flush_and_count = |batch: &mut Vec<Packet>,
                           socket: &std::net::UdpSocket,
                           addr: &Arc<ArcSwap<SocketAddr>>,
                           send_buf: &mut Vec<u8>| {
        let n = flush_batch(batch, socket, addr, send_buf, mtu, node_index);
        sent_count.fetch_add(n as u64, Ordering::Relaxed);
    };

    loop {
        if cancel.is_cancelled() {
            if !batch.is_empty() {
                flush_and_count(&mut batch, &socket, &target_addr, &mut send_buf);
            }
            break;
        }

        match rx.recv_timeout(FLUSH_DURATION) {
            Ok(packet) => {
                // When MTU is enabled and the next packet would cause the
                // batch to exceed the MTU, flush first.  Estimation is
                // conservative: 5-byte batch header + 4-byte frame_len per
                // existing packet + the new packet's likely wire size.
                if mtu > 0 && !batch.is_empty() && would_exceed_mtu(&batch, &packet, mtu) {
                    flush_and_count(&mut batch, &socket, &target_addr, &mut send_buf);
                }
                batch.push(packet);
            }
            Err(RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    flush_and_count(&mut batch, &socket, &target_addr, &mut send_buf);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    flush_and_count(&mut batch, &socket, &target_addr, &mut send_buf);
                }
                break;
            }
        }
    }
}

/// Quick estimation: would adding `next` to the existing `batch` push the
/// encoded size past `mtu`?
///
/// We use a conservative formula to avoid actually encoding:
///   total ≈ batch_header(5) + Σ(4 + frame_len) + 4 + next_frame_len
fn would_exceed_mtu(batch: &[Packet], next: &Packet, mtu: usize) -> bool {
    let max_payload = mtu.saturating_sub(28);
    // Estimate current encoded size (batch header + existing frames)
    let mut total: usize = 5; // BATCH_MAGIC(2) + VERSION(1) + count(2)
    for p in batch {
        // 4-byte frame_len prefix + encoded packet size (conservative)
        total += 4 + estimated_wire_size(p);
    }
    total += 4 + estimated_wire_size(next);
    total > max_payload
}

/// Rough upper bound on the wire size of a Packet.
/// Used by [`would_exceed_mtu`] to decide batch splitting without encoding.
fn estimated_wire_size(p: &Packet) -> usize {
    // Magic(2) + Version(1) + MsgType(1) + IpFamily(1) + SrcIp(4 or 16)
    //   + SrcPort(2) + DstIp(4 or 16) + DstPort(2) + Timestamp(8)
    //   + MetadataLen(4) + metadata(var) + PayloadLen(4) + Payload(var)
    let ip_size: usize = match p.src.0 {
        IpAddr::V4(_) => 4,
        IpAddr::V6(_) => 16,
    };
    let metadata_size = if p.call_id.is_some() || p.leg.is_some() {
        let call_id_len = p.call_id.as_ref().map(|s| s.len()).unwrap_or(0);
        4 + 4 + 4 + call_id_len // leg(i32) + call_id_len(u32) + call_id
    } else {
        4 // metadata_len = 0
    };
    2 + 1 + 1 + 1 + ip_size + 2 + ip_size + 2 + 8 + 4 + 4 + metadata_size + 4 + p.payload.len()
}

/// Encode and send the pending batch of packets.
///
/// When `mtu == 0`, all packets are encoded as a single datagram (either
/// single-packet or batch format). When `mtu > 0`, the batch is split into
/// multiple MTU-sized datagrams, each carrying a valid batch frame.
///
/// Single-packet batches use the legacy single-packet wire format so that
/// legacy receivers can still parse them without batch support.
fn flush_batch(
    batch: &mut Vec<Packet>,
    socket: &std::net::UdpSocket,
    addr: &Arc<ArcSwap<SocketAddr>>,
    send_buf: &mut Vec<u8>,
    mtu: usize,
    _node_index: usize,
) -> usize {
    if batch.is_empty() {
        return 0;
    }

    let target_addr = **addr.load();
    let total = batch.len();

    if mtu == 0 {
        // No MTU limit: send as a single datagram
        send_buf.clear();
        if batch.len() == 1 {
            encode_packet_into(send_buf, &batch[0]);
        } else if encode_batch_into(send_buf, batch).is_err() {
            // Fallback: should never happen with reasonable batch sizes
            for packet in batch.drain(..) {
                send_buf.clear();
                encode_packet_into(send_buf, &packet);
                let _ = socket.send_to(send_buf, target_addr);
            }
            batch.clear();
            return total;
        }
        let _ = socket.send_to(send_buf, target_addr);
        batch.clear();
        return total;
    }

    // MTU-aware splitting: build batches that fit within `mtu`
    let max_payload = mtu.saturating_sub(28); // IP(20) + UDP(8)
    let mut start = 0;

    while start < batch.len() {
        send_buf.clear();
        send_buf.put_u16(BATCH_MAGIC);
        send_buf.put_u8(BATCH_VERSION);
        let count_pos = send_buf.len();
        send_buf.put_u16(0); // placeholder count
        let mut count: u16 = 0;

        for i in start..batch.len() {
            let frame_start = send_buf.len();
            send_buf.put_u32(0); // placeholder frame_len
            encode_packet_into(send_buf, &batch[i]);
            let frame_len = (send_buf.len() - frame_start - 4) as u32;

            if count > 0 && send_buf.len() > max_payload {
                // This packet doesn't fit — roll back and send current batch
                send_buf.truncate(frame_start);
                break;
            }

            send_buf[frame_start..frame_start + 4].copy_from_slice(&frame_len.to_be_bytes());
            count += 1;
        }

        if count == 0 {
            // Single packet exceeds MTU (extremely rare — only for very
            // large SIP messages). Send it as a standalone single-packet
            // datagram; IP fragmentation will handle it.
            send_buf.clear();
            encode_packet_into(send_buf, &batch[start]);
            let _ = socket.send_to(send_buf, target_addr);
            start += 1;
            continue;
        }

        send_buf[count_pos..count_pos + 2].copy_from_slice(&count.to_be_bytes());
        let _ = socket.send_to(send_buf, target_addr);
        start += count as usize;
    }

    batch.clear();
    total
}

/// Background task that periodically re-resolves each node's UDP hostname.
/// When a resolved address changes, the node's [`ArcSwap`] is updated
/// atomically so that subsequent sends use the new address.
async fn dns_refresh_loop(nodes: Vec<RemoteNode>, ttl_secs: u64, cancel: CancellationToken) {
    let interval = Duration::from_secs(ttl_secs);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
        for node in &nodes {
            match lookup_host(node.udp_host.as_str()).await {
                Ok(mut addrs) => {
                    if let Some(new_addr) = addrs.next() {
                        let old = **node.udp_addr.load();
                        if old != new_addr {
                            tracing::info!(
                                old = %old,
                                new = %new_addr,
                                host = %node.udp_host,
                                "SipFlow remote node DNS updated"
                            );
                            node.udp_addr.store(Arc::new(new_addr));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        host = %node.udp_host,
                        error = %e,
                        "SipFlow DNS re-resolution failed, keeping previous address"
                    );
                }
            }
        }
    }
}

/// Periodically queries each remote node for per-client receive counters
/// and logs the loss rate.  Exports per-node loss metrics.
///
/// Each node stores a cumulative receive count keyed by `client_id`.
/// We track the previous `sent`/`recv` snapshots locally and compute
/// deltas each interval to derive the loss rate.
async fn report_loop(
    nodes: Vec<(RemoteNode, Arc<AtomicU64>)>,
    client: reqwest::Client,
    interval_secs: u64,
    client_id: u32,
    cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    let mut last_sent: Vec<u64> = vec![0; nodes.len()];
    let mut last_recv: Vec<u64> = vec![0; nodes.len()];

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {}
        }

        for (i, (node, sent_count)) in nodes.iter().enumerate() {
            let current_sent = sent_count.load(Ordering::Relaxed);
            let sent_delta = current_sent - last_sent[i];
            if sent_delta == 0 {
                continue;
            }

            let node_addr = node.http_addr.clone();
            match client
                .post(format!("{}/report", node_addr))
                .json(&serde_json::json!({
                    "client_id": client_id,
                    // Cumulative sent count so the collector can also derive a
                    // per-interval loss rate when it receives the report.
                    "sent_count": current_sent,
                }))
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        let current_recv = data["packets_received"].as_u64().unwrap_or(0);
                        let recv_delta = current_recv - last_recv[i];
                        let loss = sent_delta.saturating_sub(recv_delta);
                        let loss_rate = if sent_delta > 0 {
                            loss as f64 / sent_delta as f64
                        } else {
                            0.0
                        };
                        tracing::info!(
                            node = %node_addr,
                            client_id,
                            interval_s = interval_secs,
                            sent = sent_delta,
                            recv = recv_delta,
                            loss = loss,
                            loss_rate = loss_rate,
                            "sipflow report"
                        );
                        metrics::gauge!(
                            "sipflow_loss_rate",
                            "node" => node_addr.clone(),
                            "client_id" => client_id.to_string(),
                        )
                        .set(loss_rate);
                        metrics::counter!(
                            "sipflow_report_sent_total",
                            "node" => node_addr.clone(),
                            "client_id" => client_id.to_string(),
                        )
                        .increment(sent_delta);
                        metrics::counter!(
                            "sipflow_report_lost_total",
                            "node" => node_addr.clone(),
                            "client_id" => client_id.to_string(),
                        )
                        .increment(loss);
                        last_sent[i] = current_sent;
                        last_recv[i] = current_recv;
                    }
                    Err(e) => {
                        tracing::warn!(
                            node = %node_addr,
                            error = %e,
                            "sipflow report: failed to parse response"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        node = %node_addr,
                        error = %e,
                        "sipflow report failed"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl SipFlowBackend for RemoteBackend {
    fn record(&self, call_id: Cow<'_, str>, item: SipFlowItem) -> Result<()> {
        let is_signaling = matches!(item.msg_type, SipFlowMsgType::Sip);
        let result = self
            .sender
            .try_send(Command::RecordItem {
                call_id: call_id.into_owned(),
                item,
            })
            .map_err(|e| anyhow::anyhow!("{e}"));
        if result.is_ok() {
            if is_signaling {
                self.perf.signaling_sent.fetch_add(1, Ordering::Relaxed);
            } else {
                self.perf.media_sent.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.perf.items_dropped.fetch_add(1, Ordering::Relaxed);
        }
        result
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
            let stats_array = json["stats"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid response format"))?;

            let stats: Vec<SipFlowMediaStats> = stats_array
                .iter()
                .filter_map(|stat| match serde_json::from_value(stat.clone()) {
                    Ok(stats) => Some(stats),
                    Err(err) => {
                        tracing::warn!(
                            call_id,
                            error = %err,
                            stat = %stat,
                            "failed to deserialize remote SipFlow media stats"
                        );
                        None
                    }
                })
                .collect();

            Ok(stats)
        } else {
            Err(anyhow::anyhow!(
                "Query failed: {}",
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

        info!(url, call_id, "remote generate_wav_file: fetching");

        let fetch_result = fetch_bytes(
            &self.client,
            reqwest::Method::GET,
            &url,
            &HttpFetchOptions::new(),
        )
        .await;

        match fetch_result {
            Ok(bytes) => {
                let mut tmp = tempfile::Builder::new()
                    .prefix("sipflow_wav_")
                    .suffix(".wav")
                    .tempfile()?;
                use std::io::Write;
                tmp.write_all(&bytes)?;
                tmp.flush()?;
                info!(
                    url,
                    call_id,
                    bytes = bytes.len(),
                    "remote generate_wav_file: success"
                );
                Ok(tmp)
            }
            Err(e) => {
                warn!(url, call_id, error = %e, "remote generate_wav_file: failed");
                Err(e)
            }
        }
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
    use dashmap::DashMap;

    /// Minimal HTTP handler that serves POST /report
    /// Returns `{"status":"success","client_id":<id>,"packets_received":<counter[client_id]>}`
    async fn serve_report(listener: tokio::net::TcpListener, counters: Arc<DashMap<u32, u64>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                continue;
            }
            // Quick-and-dirty: find `{"client_id":<N>}` in the body
            let req = String::from_utf8_lossy(&buf[..n]);
            let client_id: u32 = req
                .split("client_id")
                .nth(1)
                .and_then(|s| {
                    let s = s.trim_start_matches(|c: char| !c.is_ascii_digit());
                    s.split(|c: char| !c.is_ascii_digit())
                        .next()
                        .and_then(|d| d.parse().ok())
                })
                .unwrap_or(0);

            let received = counters.get(&client_id).map(|v| *v).unwrap_or(0);
            let body = serde_json::json!({
                "status": "success",
                "client_id": client_id,
                "packets_received": received,
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    }

    #[tokio::test]
    async fn test_report_endpoint_returns_receive_counters() {
        let counters: Arc<DashMap<u32, u64>> = Arc::new(DashMap::new());
        counters.insert(42, 100);

        let bind_addr = "127.0.0.1:0";
        let listener = tokio::net::TcpListener::bind(bind_addr).await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn minimal HTTP server
        let srv_counters = counters.clone();
        tokio::spawn(async move {
            serve_report(listener, srv_counters).await;
        });

        // Generate a unique client_id for the backend so it won't collide with our manual test
        let test_client_id: u32 = 42;

        // Client: POST /report
        let client = rustpbx_http_util::build_keepalive_client(None, None).unwrap();
        let resp = client
            .post(format!("http://{}/report", addr))
            .json(&serde_json::json!({ "client_id": test_client_id }))
            .send()
            .await
            .expect("POST /report failed");

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["status"], "success");
        assert_eq!(json["client_id"], 42);
        assert_eq!(json["packets_received"], 100);
    }

    #[tokio::test]
    async fn remote_backend_uses_one_way_child_cancellation() {
        let server_cancel = CancellationToken::new();
        let backend = RemoteBackend::new(
            vec![SipFlowClusterNode {
                udp: "127.0.0.1:3000".to_string(),
                http: "http://127.0.0.1:3001".to_string(),
            }],
            1,
            16,
            0,
            0,
            0,
            server_cancel.clone(),
        )
        .await
        .expect("remote backend should be created");
        let backend_cancel = backend.cancel_token.clone();

        drop(backend);

        assert!(backend_cancel.is_cancelled());
        assert!(
            !server_cancel.is_cancelled(),
            "dropping a remote backend must not cancel the server"
        );

        let backend = RemoteBackend::new(
            vec![SipFlowClusterNode {
                udp: "127.0.0.1:3000".to_string(),
                http: "http://127.0.0.1:3001".to_string(),
            }],
            1,
            16,
            0,
            0,
            0,
            server_cancel.clone(),
        )
        .await
        .expect("remote backend should be created");
        let backend_cancel = backend.cancel_token.clone();

        server_cancel.cancel();

        assert!(backend_cancel.is_cancelled());
    }
}
