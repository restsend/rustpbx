//! Observability addon configuration
//!
//! Observability addon manages its own metrics configuration independently.

use serde::{Deserialize, Serialize};

/// Metrics configuration for Prometheus endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
    #[serde(default = "default_metrics_path")]
    pub path: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_healthz_path")]
    pub healthz_path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            path: default_metrics_path(),
            token: None,
            healthz_path: default_healthz_path(),
        }
    }
}

fn default_metrics_enabled() -> bool {
    true
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

fn default_healthz_path() -> String {
    "/healthz".to_string()
}
