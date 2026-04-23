//! ACME addon configuration
//!
//! ACME addon manages its own configuration independently.

use serde::{Deserialize, Serialize};

/// ACME Let's Encrypt configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcmeConfig {
    /// Enable automatic certificate renewal
    #[serde(default)]
    pub auto_renew: bool,
    /// Hours before expiry to trigger renewal (default: 72 hours = 3 days)
    #[serde(default = "default_acme_renewal_threshold_hours")]
    pub renewal_threshold_hours: u64,
    /// Automatically reload HTTPS after renewal
    #[serde(default)]
    pub renew_https: bool,
    /// Automatically reload SIP TLS after renewal
    #[serde(default)]
    pub renew_sips: bool,
    /// Domain to manage (if not set, will be inferred from existing certificates)
    #[serde(default)]
    pub domain: Option<String>,
}

fn default_acme_renewal_threshold_hours() -> u64 {
    72
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            auto_renew: false,
            renewal_threshold_hours: default_acme_renewal_threshold_hours(),
            renew_https: true,
            renew_sips: true,
            domain: None,
        }
    }
}
