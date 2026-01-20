use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::net::IpAddr;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::SipFlowSubdirs;
use crate::sipflow::backend::SipFlowBackend;
use crate::sipflow::protocol::{MsgType, Packet};
use crate::sipflow::storage::{StorageManager, process_packet};
use crate::sipflow::wav_utils::generate_wav_from_packets;
use crate::sipflow::{SipFlowItem, SipFlowMsgType};

enum Command {
    RecordItem { call_id: String, item: SipFlowItem },
}

/// Local (embedded) backend that runs sipflow storage in a background task
pub struct LocalBackend {
    sender: mpsc::UnboundedSender<Command>,
    root: String,
    cancel_token: CancellationToken,
}

impl LocalBackend {
    pub fn new(
        root: String,
        subdirs: SipFlowSubdirs,
        flush_count: usize,
        flush_interval_secs: u64,
        id_cache_size: usize,
    ) -> Result<Self> {
        std::fs::create_dir_all(&root)?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
        let cancel_token = CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();
        let root_clone = root.clone();
        // Spawn background worker task
        tokio::spawn(async move {
            let mut storage = StorageManager::new(
                &PathBuf::from(&root_clone),
                flush_count,
                flush_interval_secs,
                id_cache_size,
                subdirs,
            );

            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

            loop {
                tokio::select! {
                    _ = cancel_token_clone.cancelled() => {
                        let _ = storage.check_flush().await;
                        break;
                    }
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            Command::RecordItem { call_id, item } => {
                                // Convert SipFlowItem to Packet for storage
                                let parse_addr = |s: &str| -> (IpAddr, u16) {
                                    let parts: Vec<&str> = s.split(':').collect();
                                    let ip = parts[0].parse().unwrap_or(IpAddr::from([127, 0, 0, 1]));
                                    let port = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
                                    (ip, port)
                                };

                                let (src_ip, src_port) = if !item.src_addr.is_empty() {
                                    parse_addr(&item.src_addr)
                                } else {
                                    (IpAddr::from([127, 0, 0, 1]), 5060)
                                };

                                let (dst_ip, dst_port) = if !item.dst_addr.is_empty() {
                                    parse_addr(&item.dst_addr)
                                } else {
                                    (IpAddr::from([127, 0, 0, 1]), 5060)
                                };

                                let msg_type = match item.msg_type {
                                    SipFlowMsgType::Sip => MsgType::Sip,
                                    SipFlowMsgType::Rtp => MsgType::Rtp,
                                };

                                let packet = Packet {
                                    msg_type,
                                    src: (src_ip, src_port),
                                    dst: (dst_ip, dst_port),
                                    timestamp: item.timestamp,
                                    payload: item.payload,
                                };

                                let mut processed = process_packet(packet);

                                if msg_type == MsgType::Rtp {
                                    processed.callid = Some(call_id);

                                    // Parse src_addr to extract leg and real IP
                                    // Format expected: "LegA_IP:PORT" or "A_IP:PORT" or just "LegA" (legacy)
                                    let (leg_id, real_src) = if item.src_addr.starts_with("LegA_") {
                                        (Some(0), item.src_addr[5..].to_string())
                                    } else if item.src_addr.starts_with("LegB_") {
                                        (Some(1), item.src_addr[5..].to_string())
                                    } else if item.src_addr.starts_with("A_") {
                                        (Some(0), item.src_addr[2..].to_string())
                                    } else if item.src_addr.starts_with("B_") {
                                        (Some(1), item.src_addr[2..].to_string())
                                    } else if item.src_addr == "LegA" || item.src_addr == "A" {
                                        (Some(0), item.src_addr.clone())
                                    } else if item.src_addr == "LegB" || item.src_addr == "B" {
                                        (Some(1), item.src_addr.clone())
                                    } else {
                                        // Default to Leg A if no explicit leg info
                                        (Some(0), item.src_addr.clone())
                                    };

                                    processed.leg = leg_id;
                                    processed.src = real_src;
                                }

                                let _ = storage.write_processed(processed).await;
                            }
                        }
                    }
                    _ = interval.tick() => {
                        let _ = storage.check_flush().await;
                    }
                }
            }
        });

        Ok(Self {
            sender: tx,
            root,
            cancel_token,
        })
    }
}

#[async_trait]
impl SipFlowBackend for LocalBackend {
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
        let call_id = call_id.to_string();
        let root = self.root.clone();

        let mut items = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, SipFlowSubdirs::None);
            storage.query_flow(&call_id, start_time, end_time).await
        })
        .await??;

        // Sort by seq or timestamp
        items.sort_by_key(|i| i.timestamp);

        Ok(items)
    }

    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<(i32, String, usize)>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();

        let stats = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, SipFlowSubdirs::None);
            storage
                .query_media_stats(&call_id, start_time, end_time)
                .await
        })
        .await??;

        Ok(stats)
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();

        let result = tokio::task::spawn(async move {
            let mut storage =
                StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, SipFlowSubdirs::None);

            // Query media packets directly by call_id
            let packets = storage.query_media(&call_id, start_time, end_time).await?;
            if packets.is_empty() {
                return Ok(Vec::new());
            }
            generate_wav_from_packets(&packets)
        })
        .await??;

        Ok(result)
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
