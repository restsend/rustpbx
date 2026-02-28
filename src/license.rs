use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

#[cfg(feature = "commerce")]
use crate::config::LicenseConfig;

/// Stub used in non-commerce builds so the `can_enable_addon` signature
/// stays consistent without gating every call site.
#[cfg(not(feature = "commerce"))]
#[derive(Debug, Clone, Default)]
pub struct LicenseConfig;

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
}

static LICENSE_CACHE: Lazy<Mutex<HashMap<String, LicenseInfo>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Per-addon license results populated once at startup.
/// All runtime checks read from this map so no network calls happen after boot.
static STARTUP_LICENSE_RESULTS: Lazy<Mutex<HashMap<String, LicenseStatus>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// ── Free-trial helpers ───────────────────────────────────────────────────────

/// Number of free-trial days counted from the first run (flag-file creation).
const FREE_TRIAL_DAYS: i64 = 30;

/// Name of the flag file stored inside `storage_dir`.
const TRIAL_FLAG_FILE: &str = ".free_trial";

/// Storage directory used to locate the trial flag file.
/// Set once at startup by `set_storage_dir`; defaults to `"storage"`.
static STORAGE_DIR: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new("storage".to_string()));

/// Call this at startup (before license checks) so the trial flag file ends up
/// in the right place.  Corresponds to `config.storage_dir`.
pub fn set_storage_dir(dir: &str) {
    if let Ok(mut s) = STORAGE_DIR.lock() {
        *s = dir.to_string();
    }
}

/// Full path to the trial flag file.
fn trial_flag_path() -> std::path::PathBuf {
    let dir = STORAGE_DIR
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "storage".to_string());
    std::path::PathBuf::from(dir).join(TRIAL_FLAG_FILE)
}

/// Read the trial-start timestamp from the flag file.
/// If the file does not yet exist it is created with the current time → first run.
/// Returns `None` only on an unrecoverable I/O error (directory missing, etc.).
fn trial_start_time() -> Option<DateTime<Utc>> {
    let path = trial_flag_path();

    if path.exists() {
        let raw = std::fs::read_to_string(&path).ok()?;
        let ts: i64 = raw.trim().parse().ok()?;
        return DateTime::<Utc>::from_timestamp(ts, 0);
    }

    // First run: stamp the current time.
    let now = Utc::now();
    let ts = now.timestamp().to_string();
    // Create parent dirs if needed (best-effort).
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &ts)
        .map_err(|e| tracing::warn!("Could not create trial flag file {:?}: {}", path, e))
        .ok()?;
    tracing::info!("Free-trial started; flag file created at {:?}", path);
    Some(now)
}

/// How many days remain in the free-trial window (negative when expired).
pub fn free_trial_days_remaining() -> i64 {
    match trial_start_time() {
        Some(start) => {
            let trial_end = start + chrono::Duration::days(FREE_TRIAL_DAYS);
            (trial_end - Utc::now()).num_days()
        }
        // Can't create/read flag file → treat as expired to be safe.
        None => -1,
    }
}

/// Returns `true` while the installation is within its free-trial window.
pub fn is_in_free_trial() -> bool {
    free_trial_days_remaining() > 0
}

// ── Startup-cache helpers ─────────────────────────────────────────────────────

/// Store per-addon license results that were resolved at startup.
/// Called once by the addon registry after `check_all_addon_licenses`.
pub fn record_startup_results(results: HashMap<String, LicenseStatus>) {
    if let Ok(mut cache) = STARTUP_LICENSE_RESULTS.lock() {
        *cache = results;
    }
}

/// Return the startup-time license status for an addon.
/// `None` means the addon was not a commercial addon (or startup hasn't run yet).
pub fn get_license_status(addon_id: &str) -> Option<LicenseStatus> {
    STARTUP_LICENSE_RESULTS.lock().ok()?.get(addon_id).cloned()
}

// ─────────────────────────────────────────────────────────────────────────────

/// Verify a license key against the license server.
pub async fn verify_license(key: &str) -> anyhow::Result<LicenseInfo> {
    let client = reqwest::Client::new();
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

#[cfg(feature = "commerce")]
/// Verify license for a specific addon using the license config.
/// Returns Ok(LicenseInfo) if valid, Err if invalid or not found.
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

    let info = verify_license(key_value).await?;

    if !info.valid {
        anyhow::bail!("License is invalid for addon: {}", addon_id);
    }

    if is_expired(&info) {
        anyhow::bail!("License has expired for addon: {}", addon_id);
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

#[cfg(feature = "commerce")]
/// Check all commercial addons and return their license status.
/// Returns a HashMap of addon_id -> LicenseStatus.
pub async fn check_all_addon_licenses(
    addon_ids: &[String],
    license_config: &Option<LicenseConfig>,
) -> HashMap<String, LicenseStatus> {
    let mut results = HashMap::new();

    let config = match license_config {
        Some(c) => c,
        None => {
            // No license config – fall back to the free-trial window for every addon.
            for addon_id in addon_ids {
                results.insert(addon_id.clone(), make_trial_status());
            }
            return results;
        }
    };

    for addon_id in addon_ids {
        let status = match config.get_license_for_addon(addon_id) {
            Some((key_name, key_value)) => match verify_license(key_value).await {
                Ok(info) => {
                    let expired = is_expired(&info);
                    LicenseStatus {
                        key_name: key_name.to_string(),
                        valid: info.valid && !expired,
                        expired,
                        expiry: info.expiry.map(|d| d.format("%Y-%m-%d").to_string()),
                        plan: info.plan,
                        is_trial: false,
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
                    }
                }
            },
            // No key configured for this addon – fall back to the free-trial window.
            None => make_trial_status(),
        };
        results.insert(addon_id.clone(), status);
    }

    results
}

#[cfg(feature = "commerce")]
/// Build a `LicenseStatus` reflecting the current free-trial state.
fn make_trial_status() -> LicenseStatus {
    let days = free_trial_days_remaining();
    let in_trial = days > 0;
    LicenseStatus {
        key_name: "free-trial".to_string(),
        valid: in_trial,
        expired: !in_trial,
        expiry: None,
        plan: if in_trial {
            format!("trial ({} days left)", days)
        } else {
            "trial expired".to_string()
        },
        is_trial: true,
    }
}

/// Check if an addon is allowed to run (valid license or community addon).
///
/// At runtime this reads exclusively from the startup cache so no network calls
/// are made – avoiding any risk of interrupting a running service.
#[cfg(feature = "commerce")]
pub async fn can_enable_addon(
    addon_id: &str,
    is_commercial: bool,
    _license_config: &Option<LicenseConfig>,
) -> bool {
    if !is_commercial {
        // Community addons don't need a license.
        return true;
    }

    // Prefer the startup-verified result (populated by check_all_addon_licenses).
    if let Some(status) = get_license_status(addon_id) {
        return status.valid;
    }

    // Startup cache miss (called before initialize_all) – allow if in trial.
    is_in_free_trial()
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

#[cfg(test)]
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
        };
        assert!(!is_expired(&info));
    }

    #[test]
    fn test_license_cache() {
        clear_cache();

        // Insert a license into cache
        let info = LicenseInfo {
            key: "test-key".to_string(),
            valid: true,
            expiry: Some(Utc::now() + Duration::days(30)),
            plan: "pro".to_string(),
            last_checked: Utc::now(),
        };

        if let Ok(mut cache) = LICENSE_CACHE.lock() {
            cache.insert("test-key".to_string(), info.clone());
        }

        // Retrieve from cache
        let cached = get_cached_license("test-key");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().key, "test-key");

        // Clear cache
        clear_cache();
        let cached = get_cached_license("test-key");
        assert!(cached.is_none());
    }

    #[cfg(feature = "commerce")]
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
