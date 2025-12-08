use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseInfo {
    pub key: String,
    pub valid: bool,
    pub expiry: Option<DateTime<Utc>>,
    pub plan: String,
    pub last_checked: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub valid: bool,
    pub expiry: Option<DateTime<Utc>>,
    pub plan: Option<String>,
}

static LICENSE_CACHE: Lazy<Mutex<HashMap<String, LicenseInfo>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub async fn verify_license(key: &str) -> anyhow::Result<LicenseInfo> {
    let client = reqwest::Client::new();
    // In a real scenario, you might want to set a timeout
    let resp = client
        .post("https://miuda.ai/api/verify")
        .json(&serde_json::json!({ "license_key": key }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    match resp {
        Ok(response) => {
            if response.status().is_success() {
                let verify_data: VerifyResponse = response.json().await?;
                let info = LicenseInfo {
                    key: key.to_string(),
                    valid: verify_data.valid,
                    expiry: verify_data.expiry,
                    plan: verify_data.plan.unwrap_or_default(),
                    last_checked: Utc::now(),
                };

                // Update cache
                if let Ok(mut cache) = LICENSE_CACHE.lock() {
                    cache.insert(key.to_string(), info.clone());
                }

                Ok(info)
            } else {
                anyhow::bail!("Verification failed with status: {}", response.status())
            }
        }
        Err(e) => {
            // Network error, check cache
            if let Ok(cache) = LICENSE_CACHE.lock() {
                if let Some(info) = cache.get(key) {
                    // Allow if previously valid
                    tracing::warn!("Network error verifying license, using cached info: {}", e);
                    return Ok(info.clone());
                }
            }
            Err(e.into())
        }
    }
}

pub fn check_feature(_feature_name: &str) -> bool {
    // Stub implementation
    true
}
