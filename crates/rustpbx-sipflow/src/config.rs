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
        #[serde(default)]
        bucket: String,
        #[serde(default)]
        region: String,
        /// Omit (or leave empty) together with `secret_key` for anonymous /
        /// public access to S3-compatible stores that don't require keys.
        #[serde(default)]
        access_key: Option<String>,
        /// Omit (or leave empty) together with `access_key` for anonymous /
        /// public access.
        #[serde(default)]
        secret_key: Option<String>,
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
        /// Lifetime in seconds of presigned download URLs generated on demand
        /// for S3-uploaded flows. Defaults to 86400 (24h), clamped to the
        /// SigV4 7-day maximum.
        #[serde(default)]
        signed_url_expiry_secs: Option<u64>,
    },
    Http {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
        /// HTTP method (default `POST`).
        #[serde(default)]
        method: Option<String>,
        /// Default multipart field for the binary payload (per-upload
        /// overrides are used for media vs signaling).
        #[serde(default)]
        file_field: Option<String>,
        /// Multipart field carrying the payload as text (mutually exclusive
        /// with `file_field`).
        #[serde(default)]
        body_field: Option<String>,
        /// File name template sent in the multipart part.
        #[serde(default)]
        file_name: Option<String>,
        /// MIME type of the binary payload.
        #[serde(default)]
        content_type: Option<String>,
        /// Extra text form fields; values support placeholders.
        #[serde(default)]
        fields: Option<std::collections::HashMap<String, String>>,
        /// Dot-path into a JSON response holding the uploaded object URL.
        #[serde(default)]
        response_url_path: Option<String>,
        /// Business-level success rule evaluated against the JSON response.
        #[serde(default)]
        response_success: Option<rustpbx_http_util::SuccessRule>,
        /// TCP connect timeout in milliseconds.
        #[serde(default)]
        connect_timeout_ms: Option<u64>,
        /// Total request timeout in milliseconds.
        #[serde(default)]
        request_timeout_ms: Option<u64>,
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

impl SipFlowUploadConfig {
    /// Lifetime of on-demand presigned download URLs for S3-uploaded flows,
    /// clamped to the SigV4 7-day maximum
    /// ([`rustpbx_storage::MAX_PRESIGN_EXPIRY_SECS`]). Returns `None` for
    /// non-S3 backends.
    pub fn signed_url_expiry_secs(&self) -> Option<u64> {
        const DEFAULT_SIGNED_URL_EXPIRY_SECS: u64 = 86_400;
        match self {
            SipFlowUploadConfig::S3 {
                signed_url_expiry_secs,
                ..
            } => Some(
                signed_url_expiry_secs
                    .unwrap_or(DEFAULT_SIGNED_URL_EXPIRY_SECS)
                    .clamp(1, rustpbx_storage::MAX_PRESIGN_EXPIRY_SECS),
            ),
            SipFlowUploadConfig::Http { .. } => None,
        }
    }

    /// Build the generic HTTP upload config for `type = "http"`. Returns
    /// `None` for S3 or when no `url` is configured. Per-upload field names
    /// (`recording` / `signaling`) are supplied by the caller.
    pub fn http_upload_config(&self) -> Option<rustpbx_http_util::HttpUploadConfig> {
        let SipFlowUploadConfig::Http {
            url,
            headers,
            method,
            file_field,
            body_field,
            file_name,
            content_type,
            fields,
            response_url_path,
            response_success,
            connect_timeout_ms,
            request_timeout_ms,
            ..
        } = self
        else {
            return None;
        };
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        Some(rustpbx_http_util::HttpUploadConfig {
            url: url.to_string(),
            method: method.clone(),
            headers: headers.clone(),
            file_field: file_field
                .clone()
                .or_else(|| Some("recording".to_string())),
            body_field: body_field.clone(),
            file_name: file_name.clone(),
            content_type: content_type.clone(),
            fields: fields.clone(),
            response_url_path: response_url_path.clone(),
            response_success: response_success.clone(),
            connect_timeout_ms: *connect_timeout_ms,
            request_timeout_ms: *request_timeout_ms,
        })
    }
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
