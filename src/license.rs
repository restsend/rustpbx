use serde::{Deserialize, Serialize};

/// Re-export the real LicenseConfig from config for commerce builds.
#[cfg(feature = "commerce")]
pub use crate::config::LicenseConfig;

/// Stub used so the `can_enable_addon` signature stays consistent in
/// non-commerce builds.
#[cfg(not(feature = "commerce"))]
#[derive(Debug, Clone, Default)]
pub struct LicenseConfig;

/// License status for UI display (without exposing the actual key)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseStatus {
    pub key_name: String,
    pub valid: bool,
    pub expired: bool,
    pub expiry: Option<String>,
    pub plan: String,
    /// True when running under the built-in free-trial window (no key required).
    #[serde(default)]
    pub is_trial: bool,
    /// Addon IDs this license covers. `None` = all addons (unlimited scope).
    #[serde(default)]
    pub scope: Option<Vec<String>>,
}

#[cfg(not(feature = "commerce"))]
impl LicenseConfig {
    pub fn get_license_for_addon(&self, _addon_id: &str) -> Option<(String, String)> {
        None
    }

    pub fn get_addons_for_key(&self, _key_name: &str) -> Vec<&str> {
        Vec::new()
    }
}

/// Check if an addon is allowed to run (always true without commerce feature).
#[cfg(not(feature = "commerce"))]
pub async fn can_enable_addon(
    _addon_id: &str,
    _is_commercial: bool,
    _license_config: &Option<LicenseConfig>,
) -> bool {
    true
}

#[cfg(not(feature = "commerce"))]
pub fn get_license_status(_addon_id: &str) -> Option<LicenseStatus> {
    None
}

// ── Commerce-only license functionality ──────────────────────────────────────

#[cfg(feature = "commerce")]
mod inner {
    use chrono::{DateTime, Utc};
    use once_cell::sync::Lazy;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{LicenseConfig, LicenseStatus};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LicenseInfo {
        pub key: String,
        pub valid: bool,
        pub expiry: Option<DateTime<Utc>>,
        pub plan: String,
        pub last_checked: DateTime<Utc>,
        /// Addon IDs this license covers. `None` = all addons (unlimited scope).
        #[serde(default)]
        pub scope: Option<Vec<String>>,
        /// Machine-readable rejection reason when `valid` is false.
        #[serde(default)]
        pub reject_reason: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct VerifyResponse {
        pub valid: bool,
        pub expiry: Option<DateTime<Utc>>,
        pub plan: Option<String>,
        /// Addon IDs this license covers. `None` or absent = all addons.
        #[serde(default)]
        pub scope: Option<Vec<String>>,
        /// Machine-readable rejection reason when `valid` is false.
        #[serde(default)]
        pub reject_reason: Option<String>,
    }

    pub(super) static LICENSE_CACHE: Lazy<Mutex<HashMap<String, LicenseInfo>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    /// Per-addon license results populated once at startup.
    pub(super) static STARTUP_LICENSE_RESULTS: Lazy<Mutex<HashMap<String, LicenseStatus>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    // ── Startup-cache helpers ─────────────────────────────────────────────────────

    /// Store per-addon license results that were resolved at startup.
    pub fn record_startup_results(results: HashMap<String, LicenseStatus>) {
        if let Ok(mut cache) = STARTUP_LICENSE_RESULTS.lock() {
            *cache = results;
        }
    }

    /// Return the startup-time license status for an addon.
    pub fn get_license_status(addon_id: &str) -> Option<LicenseStatus> {
        STARTUP_LICENSE_RESULTS.lock().ok()?.get(addon_id).cloned()
    }

    /// Update the in-memory license status for one or more addons immediately
    /// after a successful verification (no restart required).
    pub fn update_license_status(addon_ids: &[String], status: LicenseStatus) {
        if let Ok(mut cache) = STARTUP_LICENSE_RESULTS.lock() {
            for id in addon_ids {
                cache.insert(id.clone(), status.clone());
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────────

    /// Verify a license key against the license server.
    /// If the key has already been verified this session, returns the cached result
    /// without making a network request.
    pub async fn verify_license(key: &str) -> anyhow::Result<LicenseInfo> {
        // Fast path: return cached result if already verified this session.
        if let Ok(cache) = LICENSE_CACHE.lock() {
            if let Some(info) = cache.get(key) {
                tracing::debug!(
                    "License key {}... served from cache",
                    &key[..key.len().min(8)]
                );
                return Ok(info.clone());
            }
        }

        let key_prefix = &key[..key.len().min(8)];
        tracing::info!(
            "Verifying license key {}... against https://miuda.ai/api/verify",
            key_prefix
        );

        let client = reqwest::Client::new();
        let resp = client
            .post("https://miuda.ai/api/verify")
            .json(&serde_json::json!({ "license_key": key }))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;

        match resp {
            Ok(response) => {
                let status = response.status();
                tracing::info!("License verify response status: {}", status);
                if status.is_success() {
                    let body = response.text().await?;
                    tracing::debug!("License verify response body: {}", body);
                    let verify_data: VerifyResponse = serde_json::from_str(&body).map_err(|e| {
                        anyhow::anyhow!("Failed to parse verify response: {e}, body: {body}")
                    })?;
                    let info = LicenseInfo {
                        key: key.to_string(),
                        valid: verify_data.valid,
                        expiry: verify_data.expiry,
                        plan: verify_data.plan.unwrap_or_default(),
                        last_checked: Utc::now(),
                        scope: verify_data.scope,
                        reject_reason: verify_data.reject_reason,
                    };

                    if let Ok(mut cache) = LICENSE_CACHE.lock() {
                        cache.insert(key.to_string(), info.clone());
                    }

                    Ok(info)
                } else {
                    let body = response.text().await.unwrap_or_default();
                    tracing::warn!(
                        "License verification failed: status={}, body={}",
                        status,
                        body
                    );
                    anyhow::bail!(
                        "Verification failed with status: {}, body: {}",
                        status,
                        body
                    )
                }
            }
            Err(e) => {
                tracing::error!("License verification network error: {}", e);
                if let Ok(cache) = LICENSE_CACHE.lock() {
                    if let Some(info) = cache.get(key) {
                        tracing::warn!("Network error verifying license, using cached info: {}", e);
                        return Ok(info.clone());
                    }
                }
                Err(e.into())
            }
        }
    }

    /// Check if a license key is expired.
    pub fn is_expired(info: &LicenseInfo) -> bool {
        if let Some(expiry) = info.expiry {
            expiry < Utc::now()
        } else {
            false
        }
    }

    /// Get cached license info if available.
    pub fn get_cached_license(key: &str) -> Option<LicenseInfo> {
        LICENSE_CACHE.lock().ok()?.get(key).cloned()
    }

    /// Clear the license cache.
    pub fn clear_cache() {
        if let Ok(mut cache) = LICENSE_CACHE.lock() {
            cache.clear();
        }
    }

    /// Verify license for a specific addon using the license config.
    pub async fn verify_addon_license(
        addon_id: &str,
        license_config: &Option<LicenseConfig>,
    ) -> anyhow::Result<LicenseInfo> {
        let config = license_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No license configuration found"))?;

        let (key_name, key_value) = config
            .get_license_for_addon(addon_id)
            .ok_or_else(|| anyhow::anyhow!("No license key configured for addon: {}", addon_id))?;

        let info = verify_license(&key_value).await?;

        if !info.valid {
            anyhow::bail!("License is invalid for addon: {}", addon_id);
        }

        if is_expired(&info) {
            anyhow::bail!("License has expired for addon: {}", addon_id);
        }

        if let Some(ref scope) = info.scope {
            if !scope.is_empty() && !scope.contains(&addon_id.to_string()) {
                anyhow::bail!(
                    "License scope {:?} does not cover addon: {}",
                    scope,
                    addon_id
                );
            }
        }

        tracing::info!(
            "License verified for addon {} with key {}: valid={}, expiry={:?}",
            addon_id,
            key_name,
            info.valid,
            info.expiry
        );

        Ok(info)
    }

    /// Check all commercial addons and return their license status.
    pub async fn check_all_addon_licenses(
        addon_ids: &[String],
        license_config: &Option<LicenseConfig>,
    ) -> HashMap<String, LicenseStatus> {
        let mut results = HashMap::new();

        let config = match license_config {
            Some(c) => c,
            None => return results,
        };

        for addon_id in addon_ids {
            let status = match config.get_license_for_addon(addon_id) {
                Some((key_name, key_value)) => match verify_license(&key_value).await {
                    Ok(info) => {
                        let expired = is_expired(&info);
                        LicenseStatus {
                            key_name: key_name.to_string(),
                            valid: info.valid && !expired,
                            expired,
                            expiry: info.expiry.map(|d| d.format("%Y-%m-%d").to_string()),
                            plan: info.plan,
                            is_trial: false,
                            scope: info.scope,
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to verify license for {}: {}", addon_id, e);
                        LicenseStatus {
                            key_name: key_name.to_string(),
                            valid: false,
                            expired: false,
                            expiry: None,
                            plan: "".to_string(),
                            is_trial: false,
                            scope: None,
                        }
                    }
                },
                None => continue,
            };
            results.insert(addon_id.clone(), status);
        }

        results
    }

    /// Check if an addon is allowed to run.
    pub async fn can_enable_addon(
        _addon_id: &str,
        _is_commercial: bool,
        _license_config: &Option<LicenseConfig>,
    ) -> bool {
        true
    }
}

#[cfg(feature = "commerce")]
pub use inner::{
    LicenseInfo, VerifyResponse, can_enable_addon, check_all_addon_licenses, clear_cache,
    get_cached_license, get_license_status, is_expired, record_startup_results,
    update_license_status, verify_addon_license, verify_license,
};

#[cfg(test)]
#[cfg(feature = "commerce")]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn test_is_expired_with_future_expiry() {
        let info = LicenseInfo {
            key: "test-key".to_string(),
            valid: true,
            expiry: Some(Utc::now() + Duration::days(30)),
            plan: "pro".to_string(),
            last_checked: Utc::now(),
            scope: None,
            reject_reason: None,
        };
        assert!(!is_expired(&info));
    }

    #[test]
    fn test_is_expired_with_past_expiry() {
        let info = LicenseInfo {
            key: "test-key".to_string(),
            valid: true,
            expiry: Some(Utc::now() - Duration::days(1)),
            plan: "pro".to_string(),
            last_checked: Utc::now(),
            scope: None,
            reject_reason: None,
        };
        assert!(is_expired(&info));
    }

    #[test]
    fn test_is_expired_with_no_expiry() {
        let info = LicenseInfo {
            key: "test-key".to_string(),
            valid: true,
            expiry: None,
            plan: "pro".to_string(),
            last_checked: Utc::now(),
            scope: None,
            reject_reason: None,
        };
        assert!(!is_expired(&info));
    }

    #[test]
    fn test_license_cache() {
        clear_cache();

        let info = LicenseInfo {
            key: "test-key".to_string(),
            valid: true,
            expiry: Some(Utc::now() + Duration::days(30)),
            plan: "pro".to_string(),
            last_checked: Utc::now(),
            scope: None,
            reject_reason: None,
        };

        if let Ok(mut cache) = inner::LICENSE_CACHE.lock() {
            cache.insert("test-key".to_string(), info.clone());
        }

        let cached = get_cached_license("test-key");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().key, "test-key");

        clear_cache();
        let cached = get_cached_license("test-key");
        assert!(cached.is_none());
    }

    mod commerce_tests {
        use crate::config::LicenseConfig;

        #[test]
        fn test_license_config_get_license_for_addon() {
            let mut config = LicenseConfig::default();
            config
                .addons
                .insert("wholesale".to_string(), "enterprise".to_string());
            config
                .keys
                .insert("enterprise".to_string(), "test-key-123".to_string());

            let result = config.get_license_for_addon("wholesale");
            assert!(result.is_some());
            let (key_name, key_value) = result.unwrap();
            assert_eq!(key_name, "enterprise");
            assert_eq!(key_value, "test-key-123");
        }

        #[test]
        fn test_license_config_get_license_for_addon_not_found() {
            let config = LicenseConfig::default();
            let result = config.get_license_for_addon("wholesale");
            assert!(result.is_none());
        }

        #[test]
        fn test_license_config_get_addons_for_key() {
            let mut config = LicenseConfig::default();
            config
                .addons
                .insert("wholesale".to_string(), "enterprise".to_string());
            config
                .addons
                .insert("endpoint-manager".to_string(), "enterprise".to_string());
            config
                .addons
                .insert("voicemail".to_string(), "basic".to_string());

            let addons = config.get_addons_for_key("enterprise");
            assert_eq!(addons.len(), 2);
            assert!(addons.contains(&"wholesale"));
            assert!(addons.contains(&"endpoint-manager"));

            let basic_addons = config.get_addons_for_key("basic");
            assert_eq!(basic_addons.len(), 1);
            assert!(basic_addons.contains(&"voicemail"));
        }

        #[test]
        fn test_license_config_empty() {
            let config = LicenseConfig::default();
            assert!(config.addons.is_empty());
            assert!(config.keys.is_empty());
        }
    }
}
