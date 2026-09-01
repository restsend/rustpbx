use serde::{Deserialize, Serialize};

pub use rustpbx_storage::S3Vendor;

fn default_true() -> Option<bool> {
    Some(true)
}

fn default_pcm_rate() -> Option<u32> {
    Some(16000)
}

fn default_sipflow_flush_count() -> usize {
    1000
}

fn default_sipflow_flush_interval() -> u64 {
    5
}

fn default_remote_channel_capacity() -> usize {
    40000
}

fn default_mtu() -> usize {
    1500
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

fn default_sipflow_shards() -> usize {
    4
}

#[derive(Debug, Deserialize, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SipFlowSubdirs {
    None,
    #[default]
    Daily,
    Hourly,
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
        #[serde(default = "default_sipflow_compress")]
        compress: bool,
        #[serde(default = "default_sipflow_compress_level")]
        compress_level: u32,
        #[serde(default = "default_sipflow_shards")]
        shards: usize,
        #[serde(default)]
        upload: Option<SipFlowUploadConfig>,
        /// When true, `record()` blocks (up to 1s) on a full worker channel
        /// instead of dropping the record immediately. The non-blocking
        /// default keeps a saturated shard from stalling the whole ingest
        /// pipeline; embedded callers that prefer bounded backpressure over
        /// drops can opt in.
        #[serde(default)]
        blocking_backpressure: bool,
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

#[cfg(test)]
mod tests {
    use super::SipFlowConfig;

    #[test]
    fn local_config_accepts_retired_flowdb_options_as_sqlite_config() {
        let config: SipFlowConfig = serde_json::from_value(serde_json::json!({
            "type": "local",
            "root": "./sipflow",
            "engine": "flowdb",
            "ttl_secs": 86400,
            "memtable_size_mb": 32,
            "block_cache_capacity_mb": 64,
            "flowdb_sync_mode": "always"
        }))
        .expect("legacy local SipFlow config should deserialize");

        let SipFlowConfig::Local {
            root,
            flush_count,
            flush_interval_secs,
            ..
        } = config
        else {
            panic!("expected local SipFlow config");
        };
        assert_eq!(root, "./sipflow");
        assert_eq!(flush_count, 1000);
        assert_eq!(flush_interval_secs, 5);
    }

    #[test]
    fn remote_config_defaults_mtu_to_standard_ethernet() {
        let config: SipFlowConfig = serde_json::from_value(serde_json::json!({
            "type": "remote",
            "udp_addr": "127.0.0.1:3000",
            "http_addr": "http://127.0.0.1:3001"
        }))
        .expect("remote SipFlow config should deserialize");

        let SipFlowConfig::Remote { mtu, .. } = config else {
            panic!("expected remote SipFlow config");
        };
        assert_eq!(mtu, 1500);
    }

    #[test]
    fn remote_config_allows_disabling_mtu_splitting() {
        let config: SipFlowConfig = serde_json::from_value(serde_json::json!({
            "type": "remote",
            "mtu": 0
        }))
        .expect("remote SipFlow config should deserialize");

        let SipFlowConfig::Remote { mtu, .. } = config else {
            panic!("expected remote SipFlow config");
        };
        assert_eq!(mtu, 0);
    }
}
