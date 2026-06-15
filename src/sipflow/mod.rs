pub mod backend;
pub mod flowdb_backend;
pub mod flowdb_codec;
pub mod perf;
pub mod protocol;
pub mod rtp_stats;
pub mod sdp_utils;
pub mod storage;
pub mod wav_utils;

use anyhow::Result;
use bytes::Bytes;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

pub use backend::{SipFlowBackend, create_backend};
pub use protocol::{MsgType, Packet, parse_packet};
pub use sdp_utils::{extract_call_id, extract_rtp_addr, extract_sdp};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SipFlowMsgType {
    Sip,
    Rtp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipFlowItem {
    pub timestamp: u64,
    #[serde(default)]
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg: Option<i32>,
    #[serde(default = "default_msg_type")]
    pub msg_type: SipFlowMsgType,
    #[serde(default)]
    pub src_addr: String,
    #[serde(default)]
    pub dst_addr: String,
    pub payload: Bytes,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SipFlowMediaStats {
    pub leg: i32,
    pub src: String,
    #[serde(default, alias = "count")]
    pub packet_count: usize,
    #[serde(default)]
    pub lost_packets: u64,
    #[serde(default)]
    pub expected_packets: u64,
    #[serde(default)]
    pub loss_percent: f64,
    #[serde(default)]
    pub jitter_ms: Option<f64>,
    #[serde(default)]
    pub ssrc: Option<u32>,
    #[serde(default)]
    pub payload_type: Option<u8>,
    #[serde(default)]
    pub clock_rate: Option<u32>,
}

fn default_msg_type() -> SipFlowMsgType {
    SipFlowMsgType::Sip
}

impl SipFlowItem {
    pub fn message_text(&self) -> Option<String> {
        if self.msg_type == SipFlowMsgType::Sip && !self.payload.is_empty() {
            Some(String::from_utf8_lossy(&self.payload).to_string())
        } else {
            None
        }
    }
}

/// Query interface for SipFlow data
pub struct SipFlowQuery {
    backend: Box<dyn SipFlowBackend>,
}

impl SipFlowQuery {
    pub fn new(backend: Box<dyn SipFlowBackend>) -> Self {
        Self { backend }
    }

    pub async fn get_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        self.backend.query_flow(call_id, start_time, end_time).await
    }

    pub async fn get_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        self.backend
            .query_media(call_id, start_time, end_time)
            .await
    }

    pub fn export_jsonl(flow: &[SipFlowItem]) -> String {
        flow.iter()
            .filter_map(|item| {
                let payload_str = String::from_utf8_lossy(&item.payload);
                let obj = serde_json::json!({
                    "timestamp": item.timestamp,
                    "seq": item.seq,
                    "leg": item.leg,
                    "msg_type": item.msg_type,
                    "src_addr": item.src_addr,
                    "dst_addr": item.dst_addr,
                    "payload": payload_str,
                });
                serde_json::to_string(&obj).ok()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests;
