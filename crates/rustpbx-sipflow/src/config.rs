use serde::{Deserialize, Serialize};

pub use rustpbx_storage::S3Vendor;

fn default_true() -> Option<bool> {
    Some(true)
}

fn default_pcm_rate() -> Option<u32> {
    Some(16000)
}

fn default_sipflow_flush_count() -> usize {
    0
}

fn default_sipflow_flush_interval() -> u64 {
    0
}

fn default_remote_channel_capacity() -> usize {
    40000
}

fn default_mtu() -> usize {
    0
}

fn default_report_interval_secs() -> u64 {
    10
}

fn default_sipflow_timeout() -> u64 {
    10
}

fn default_sipflow_dns_ttl() -> u64 {
    5
}

fn default_sipflow_id_cache_size() -> usize {
    8192
}

fn default_sipflow_compress() -> bool {
    true
}

fn default_sipflow_compress_level() -> u32 {
    6
}

fn default_flowdb_memtable_mb() -> usize {
    64
}

fn default_flowdb_block_cache_mb() -> usize {
    128
}

#[derive(Debug, Deserialize, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SipFlowSubdirs {
    None,
    #[default]
    Daily,
    Hourly,
}

#[derive(Debug, Deserialize, Clone, Copy, Serialize, PartialEq, Eq, Default)]
pub enum SipFlowEngine {
    #[serde(rename = "flowdb")]
    FlowDb,
    #[default]
    #[serde(rename = "sqlite")]
    Sqlite,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SipFlowUploadConfig {
    S3 {
        vendor: S3Vendor,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        endpoint: String,
        root: String,
        #[serde(default)]
        signaling: Option<bool>,
        #[serde(default = "default_true")]
        media: Option<bool>,
        #[serde(default)]
        force_pcm: Option<bool>,
        #[serde(default = "default_pcm_rate")]
        pcm_sample_rate: Option<u32>,
    },
    Http {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        signaling: Option<bool>,
        #[serde(default = "default_true")]
        media: Option<bool>,
        #[serde(default)]
        force_pcm: Option<bool>,
        #[serde(default = "default_pcm_rate")]
        pcm_sample_rate: Option<u32>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct SipFlowClusterNode {
    pub udp: String,
    pub http: String,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SipFlowConfig {
    Local {
        root: String,
        #[serde(default)]
        subdirs: SipFlowSubdirs,
        #[serde(default = "default_sipflow_flush_count")]
        flush_count: usize,
        #[serde(default = "default_sipflow_flush_interval")]
        flush_interval_secs: u64,
        #[serde(default = "default_sipflow_id_cache_size")]
        id_cache_size: usize,
        #[serde(default)]
        engine: SipFlowEngine,
        #[serde(default = "default_sipflow_compress")]
        compress: bool,
        #[serde(default = "default_sipflow_compress_level")]
        compress_level: u32,
        #[serde(default)]
        ttl_secs: Option<u64>,
        #[serde(default = "default_flowdb_memtable_mb")]
        memtable_size_mb: usize,
        #[serde(default = "default_flowdb_block_cache_mb")]
        block_cache_capacity_mb: usize,
        #[serde(default)]
        upload: Option<SipFlowUploadConfig>,
    },
    Remote {
        #[serde(default)]
        nodes: Vec<SipFlowClusterNode>,
        #[serde(default)]
        udp_addr: Option<String>,
        #[serde(default)]
        http_addr: Option<String>,
        #[serde(default = "default_sipflow_timeout")]
        timeout_secs: u64,
        #[serde(default = "default_remote_channel_capacity")]
        channel_capacity: usize,
        #[serde(default = "default_sipflow_dns_ttl")]
        dns_ttl_secs: u64,
        #[serde(default = "default_mtu")]
        mtu: usize,
        #[serde(default = "default_report_interval_secs")]
        report_interval_secs: u64,
        #[serde(default)]
        upload: Option<SipFlowUploadConfig>,
        #[serde(default)]
        delegate_upload: bool,
    },
}
