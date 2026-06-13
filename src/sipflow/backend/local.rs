use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::net::IpAddr;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::SipFlowSubdirs;
use crate::sipflow::backend::SipFlowBackend;
use crate::sipflow::protocol::{MsgType, Packet};
use crate::sipflow::storage::{StorageManager, process_packet};
use crate::sipflow::wav_utils::{
    generate_wav_to_writer,
};
use crate::sipflow::{SipFlowItem, SipFlowMediaStats, SipFlowMsgType};

enum Command {
    RecordItem {
        call_id: String,
        item: SipFlowItem,
    },
    Flush {
        done: tokio::sync::oneshot::Sender<()>,
    },
}

/// Local (embedded) backend that runs sipflow storage in a background task
pub struct LocalBackend {
    sender: mpsc::UnboundedSender<Command>,
    root: String,
    subdirs: SipFlowSubdirs,
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
        let subdirs_clone = subdirs.clone();
        // Spawn background worker task
        crate::utils::spawn(async move {
            let mut storage = StorageManager::new(
                &PathBuf::from(&root_clone),
                flush_count,
                flush_interval_secs,
                id_cache_size,
                subdirs_clone,
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

                                let processed = process_packet(packet);
                                let _ = storage.write_processed(processed).await;
                            }
                            Command::Flush { done } => {
                                let _ = storage.force_flush().await;
                                let _ = done.send(());
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
            subdirs,
            cancel_token,
        })
    }
}

#[async_trait]
impl SipFlowBackend for LocalBackend {
    async fn flush(&self) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .sender
            .send(Command::Flush { done: tx })
            .is_err()
        {
            warn!("SipFlowBackend flush: worker channel closed, skipping flush");
            return Ok(());
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => {
                warn!("SipFlowBackend flush: oneshot cancelled");
                Ok(())
            }
            Err(_) => {
                warn!("SipFlowBackend flush: timed out after 30s");
                Ok(())
            }
        }
    }

    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()> {
        self.sender
            .send(Command::RecordItem {
                call_id: call_id.to_string(),
                item,
            })
            .map_err(|e| anyhow::anyhow!("Failed to send record command: {}", e))?;
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
        let subdirs = self.subdirs.clone();

        let mut items = tokio::task::spawn(async move {
            let mut storage = StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, subdirs);
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
    ) -> Result<Vec<SipFlowMediaStats>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();

        let stats = tokio::task::spawn(async move {
            let mut storage = StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, subdirs);
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
        let subdirs = self.subdirs.clone();

        let result = tokio::task::spawn(async move {
            let mut storage = StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, subdirs);
            let packets = storage.query_media(&call_id, start_time, end_time).await?;
            if packets.is_empty() {
                return Ok(Vec::<u8>::new());
            }
            let payload_map = build_payload_maps(&mut storage, &call_id, start_time, end_time).await;
            let mut cursor = std::io::Cursor::new(Vec::new());
            generate_wav_to_writer(&packets, &payload_map.0, &payload_map.1, true, &mut cursor)?;
            Ok::<Vec<u8>, anyhow::Error>(cursor.into_inner())
        })
        .await??;

        Ok(result)
    }

    async fn query_media_stream(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<Vec<u8>> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();

        let result = tokio::task::spawn(async move {
            let mut storage = StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, subdirs);
            let mut packets = storage.query_media(&call_id, start_time, end_time).await?;
            if let Some(leg) = stream_leg {
                packets.retain(|(packet_leg, _, _)| *packet_leg == leg);
            }
            if packets.is_empty() {
                return Ok::<Vec<u8>, anyhow::Error>(Vec::new());
            }
            let payload_map = build_payload_maps_filtered(
                &mut storage,
                &call_id,
                start_time,
                end_time,
                stream_leg,
            )
            .await;
            let mut cursor = std::io::Cursor::new(Vec::new());
            generate_wav_to_writer(&packets, &payload_map.0, &payload_map.1, true, &mut cursor)?;
            Ok::<Vec<u8>, anyhow::Error>(cursor.into_inner())
        })
        .await??;

        Ok(result)
    }

    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        let call_id = call_id.to_string();
        let root = self.root.clone();
        let subdirs = self.subdirs.clone();

        let file = tokio::task::spawn(async move {
            let mut storage = StorageManager::new(&PathBuf::from(&root), 1000, 5, 1024, subdirs);
            let mut packets = storage.query_media(&call_id, start_time, end_time).await?;
            if let Some(leg) = stream_leg {
                packets.retain(|(packet_leg, _, _)| *packet_leg == leg);
            }
            if packets.is_empty() {
                return Ok::<Option<tempfile::NamedTempFile>, anyhow::Error>(None);
            }
            let payload_map = build_payload_maps_filtered(
                &mut storage,
                &call_id,
                start_time,
                end_time,
                stream_leg,
            )
            .await;

            let mut file = tempfile::NamedTempFile::new()?;
            generate_wav_to_writer(&packets, &payload_map.0, &payload_map.1, true, &mut file)?;
            std::io::Write::flush(&mut file)?;
            Ok::<Option<tempfile::NamedTempFile>, anyhow::Error>(Some(file))
        })
        .await??
        .ok_or_else(|| anyhow::anyhow!("No media packets found"))?;

        Ok(file)
    }
}

async fn build_payload_maps(
    storage: &mut StorageManager,
    call_id: &str,
    start_time: DateTime<Local>,
    end_time: DateTime<Local>,
) -> (crate::sipflow::wav_utils::PayloadTypeMap, crate::sipflow::wav_utils::LegPayloadTypeMap) {
    use crate::sipflow::wav_utils::{build_payload_type_map, build_payload_type_map_by_leg};
    let media_sources = storage
        .query_media_sources(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let mut leg_sources = std::collections::HashMap::<i32, Vec<String>>::new();
    for source in media_sources {
        leg_sources.entry(source.leg).or_default().push(source.src);
    }
    let flow = storage
        .query_flow(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let payload_map = build_payload_type_map(&flow);
    let leg_payload_map = build_payload_type_map_by_leg(&flow, &leg_sources);
    (payload_map, leg_payload_map)
}

type LegPayloadTypeMap = crate::sipflow::wav_utils::LegPayloadTypeMap;

async fn build_payload_maps_filtered(
    storage: &mut StorageManager,
    call_id: &str,
    start_time: DateTime<Local>,
    end_time: DateTime<Local>,
    stream_leg: Option<i32>,
) -> (crate::sipflow::wav_utils::PayloadTypeMap, LegPayloadTypeMap) {
    use crate::sipflow::wav_utils::{build_payload_type_map, build_payload_type_map_by_leg};
    let media_sources = storage
        .query_media_sources(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let mut leg_sources = std::collections::HashMap::<i32, Vec<String>>::new();
    for source in media_sources {
        if stream_leg.is_none_or(|selected| selected == source.leg) {
            leg_sources.entry(source.leg).or_default().push(source.src);
        }
    }
    let flow = storage
        .query_flow(call_id, start_time, end_time)
        .await
        .unwrap_or_default();
    let payload_map = build_payload_type_map(&flow);
    let leg_payload_map = build_payload_type_map_by_leg(&flow, &leg_sources);
    (payload_map, leg_payload_map)
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
