use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::sipflow::backend::SipFlowBackend;
use crate::sipflow::protocol::{MsgType, Packet, encode_packet};
use crate::sipflow::{SipFlowItem, SipFlowMsgType};

enum Command {
    RecordItem { item: SipFlowItem },
}

/// Remote backend that sends data to a remote sipflow server via UDP
pub struct RemoteBackend {
    sender: mpsc::UnboundedSender<Command>,
    http_addr: String,
    client: reqwest::Client,
    cancel_token: CancellationToken,
}

impl RemoteBackend {
    pub fn new(udp_addr: String, http_addr: String, timeout_secs: u64) -> Result<Self> {
        // Bind to any local address
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        let udp_socket = Arc::new(UdpSocket::from_std(socket)?);
        let target_addr: SocketAddr = udp_addr.parse()?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
        let cancel_token = CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token_clone.cancelled() => {
                        break;
                    }
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            Command::RecordItem { item } => {
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

                                let data = encode_packet(&packet);
                                // Non-blocking send, ignore errors (fire and forget)
                                let _ = udp_socket.send_to(&data, target_addr).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            sender: tx,
            http_addr,
            client,
            cancel_token,
        })
    }
}

#[async_trait]
impl SipFlowBackend for RemoteBackend {
    fn record(&self, _call_id: &str, item: SipFlowItem) -> Result<()> {
        self.sender.send(Command::RecordItem { item })?;
        Ok(())
    }

    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let url = format!(
            "{}/flow?callid={}&start={}&end={}",
            self.http_addr,
            call_id,
            start_time.timestamp(),
            end_time.timestamp()
        );

        let response = self.client.get(&url).send().await?;
        let json: serde_json::Value = response.json().await?;

        if json["status"] == "success" {
            let flow_array = json["flow"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid response format: flow is not an array"))?;

            let mut items: Vec<SipFlowItem> = flow_array
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect();

            // Sort by timestamp
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
        _call_id: &str,
        _start_time: DateTime<Local>,
        _end_time: DateTime<Local>,
    ) -> Result<Vec<(i32, String, usize)>> {
        // TODO: Implement remote media stats query
        Ok(Vec::new())
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}/media?callid={}&start={}&end={}",
            self.http_addr,
            call_id,
            start_time.timestamp(),
            end_time.timestamp()
        );

        let response = self.client.get(&url).send().await?;

        if response.status().is_success() {
            let bytes = response.bytes().await?;
            Ok(bytes.to_vec())
        } else {
            Err(anyhow::anyhow!("Media query failed: {}", response.status()))
        }
    }
}

impl Drop for RemoteBackend {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
