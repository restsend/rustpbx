use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub use crate::config::LicenseConfig;

pub const OFFLINE_TOKEN_PREFIX: &str = "PBX1.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseStatus {
    pub key_name: String,
    pub valid: bool,
    pub expired: bool,
    pub expiry: Option<String>,
    pub plan: String,
    #[serde(default)]
    pub is_trial: bool,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
}

impl LicenseStatus {
    pub fn days_until_expiry(&self) -> Option<i64> {
        let date = chrono::NaiveDate::parse_from_str(self.expiry.as_deref()?, "%Y-%m-%d").ok()?;
        let today = chrono::Utc::now().date_naive();
        Some((date - today).num_days())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseInfo {
    pub key: String,
    pub valid: bool,
    pub expiry: Option<chrono::DateTime<chrono::Utc>>,
    pub plan: String,
    pub last_checked: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    #[serde(default)]
    pub reject_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub valid: bool,
    pub expiry: Option<chrono::DateTime<chrono::Utc>>,
    pub plan: Option<String>,
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    #[serde(default)]
    pub reject_reason: Option<String>,
}

/// Claims carried by an offline token payload.
///
/// Minted tokens may also carry a `kid` (signing key id) claim; it is ignored
/// here because verification always pins the single `[licenses] public_key`
/// configured for this deployment.
#[derive(Debug, Clone, serde::Deserialize)]
struct OfflineClaims {
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    scope: Option<Vec<String>>,
}

pub(crate) const LICENSE_CACHE_TTL_SECS: i64 = 3600;

static LICENSE_CACHE: once_cell::sync::Lazy<dashmap::DashMap<String, LicenseInfo>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

static STARTUP_LICENSE_RESULTS: once_cell::sync::Lazy<dashmap::DashMap<String, LicenseStatus>> =
    once_cell::sync::Lazy::new(dashmap::DashMap::new);

fn trial_status() -> LicenseStatus {
    LicenseStatus {
        key_name: "trial".to_string(),
        valid: true,
        expired: false,
        expiry: None,
        plan: "trial".to_string(),
        is_trial: true,
        scope: None,
    }
}

fn denied_status(key_name: String) -> LicenseStatus {
    LicenseStatus {
        key_name,
        valid: false,
        expired: false,
        expiry: None,
        plan: String::new(),
        is_trial: false,
        scope: None,
    }
}

pub fn record_startup_results(results: std::collections::HashMap<String, LicenseStatus>) {
    STARTUP_LICENSE_RESULTS.clear();
    for (k, v) in results {
        STARTUP_LICENSE_RESULTS.insert(k, v);
    }
}

pub fn get_license_status(addon_id: &str) -> Option<LicenseStatus> {
    STARTUP_LICENSE_RESULTS.get(addon_id).map(|r| r.clone())
}

pub fn update_license_status(addon_ids: &[String], status: LicenseStatus) {
    for id in addon_ids {
        STARTUP_LICENSE_RESULTS.insert(id.clone(), status.clone());
    }
}

fn cached_license_for_verify(key: &str) -> Option<LicenseInfo> {
    let entry = LICENSE_CACHE.get(key).map(|r| r.clone());
    let info = entry?;
    let age = chrono::Utc::now() - info.last_checked;
    if age < chrono::Duration::seconds(LICENSE_CACHE_TTL_SECS) {
        return Some(info);
    }
    tracing::debug!(
        "License key {}... cache entry older than TTL, verifying online",
        &key[..key.len().min(8)]
    );
    LICENSE_CACHE.remove(key);
    None
}

pub async fn verify_license(key: &str, email: Option<&str>) -> anyhow::Result<LicenseInfo> {
    if let Some(info) = cached_license_for_verify(key) {
        tracing::debug!(
            "License key {}... served from cache",
            &key[..key.len().min(8)]
        );
        return Ok(info);
    }

    let key_prefix = &key[..key.len().min(8)];
    tracing::info!(
        "Verifying license key {}... against https://miuda.ai/api/verify",
        key_prefix
    );

    let mut body = serde_json::json!({ "license_key": key });
    if let Some(email) = email {
        body["email"] = serde_json::Value::String(email.to_string());
    }
    let opts = crate::http_util::HttpFetchOptions::new()
        .with_timeout(std::time::Duration::from_secs(5));
    let req = crate::http_util::shared_keepalive_client()
        .post("https://miuda.ai/api/verify")
        .json(&body);
    match crate::http_util::execute_request(req, &opts.headers, opts.timeout).await {
        Ok(response) => {
            let status = response.status();
            tracing::info!("License verify response status: {}", status);
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
                last_checked: chrono::Utc::now(),
                scope: verify_data.scope,
                reject_reason: verify_data.reject_reason,
            };

            LICENSE_CACHE.insert(key.to_string(), info.clone());

            Ok(info)
        }
        Err(e) => {
            tracing::error!("License verification error: {}", e);
            if let Some(info) = LICENSE_CACHE.get(key) {
                tracing::warn!("Network error verifying license, using cached info: {}", e);
                return Ok(info.clone());
            }
            Err(e)
        }
    }
}

pub fn is_expired(info: &LicenseInfo) -> bool {
    if let Some(expiry) = info.expiry {
        expiry < chrono::Utc::now()
    } else {
        false
    }
}

pub fn expiring_soon(status: &LicenseStatus, threshold_days: i64) -> bool {
    status.valid
        && !status.expired
        && status
            .days_until_expiry()
            .map(|days| days <= threshold_days)
            .unwrap_or(false)
}

pub fn get_cached_license(key: &str) -> Option<LicenseInfo> {
    LICENSE_CACHE.get(key).map(|r| r.clone())
}

pub fn clear_cache() {
    LICENSE_CACHE.clear();
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Verify a self-contained offline license token: `PBX1.<payload>.<signature>`
/// where the payload is URL-safe base64 of the JSON claims and the signature is
/// URL-safe base64 of an Ed25519 signature over the raw payload bytes. The
/// public key (32-byte hex) is supplied by configuration, never hardcoded.
pub fn verify_offline(key: &str, public_key_hex: Option<&str>) -> Option<LicenseInfo> {
    let rest = key.strip_prefix(OFFLINE_TOKEN_PREFIX)?;
    let mut parts = rest.split('.');
    let payload_b64 = parts.next()?;
    let sig_b64 = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig_b64)
        .ok()?;

    let pubkey = decode_hex(public_key_hex?)?;
    if pubkey.len() != 32 {
        return None;
    }

    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pubkey.as_slice())
        .verify(&payload, &sig)
        .ok()?;

    let claims: OfflineClaims = serde_json::from_slice(&payload).ok()?;
    let expiry = claims
        .exp
        .and_then(|e| chrono::DateTime::<chrono::Utc>::from_timestamp(e, 0));

    Some(LicenseInfo {
        key: key.to_string(),
        valid: true,
        expiry,
        plan: claims.plan.unwrap_or_default(),
        last_checked: chrono::Utc::now(),
        scope: claims.scope,
        reject_reason: None,
    })
}

pub struct LicenseKeypair {
    pub public_key_hex: String,
    pub private_key_hex: String,
}

pub fn generate_keypair() -> anyhow::Result<LicenseKeypair> {
    use ring::signature::KeyPair as _;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("failed to generate ed25519 keypair"))?;
    let keypair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow::anyhow!("failed to load generated keypair"))?;
    Ok(LicenseKeypair {
        public_key_hex: encode_hex(keypair.public_key().as_ref()),
        private_key_hex: encode_hex(pkcs8.as_ref()),
    })
}

pub fn public_key_from_private(private_key_hex: &str) -> anyhow::Result<String> {
    use ring::signature::KeyPair as _;
    let pkcs8 =
        decode_hex(private_key_hex).ok_or_else(|| anyhow::anyhow!("invalid private key hex"))?;
    let keypair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid ed25519 private key"))?;
    Ok(encode_hex(keypair.public_key().as_ref()))
}

pub fn mint_offline_token(
    private_key_hex: &str,
    plan: &str,
    scope: &[String],
    expires_at: Option<i64>,
    kid: Option<&str>,
) -> anyhow::Result<String> {
    let pkcs8 =
        decode_hex(private_key_hex).ok_or_else(|| anyhow::anyhow!("invalid private key hex"))?;
    let keypair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid ed25519 private key"))?;

    let mut claims = serde_json::Map::new();
    claims.insert(
        "plan".to_string(),
        serde_json::Value::String(plan.to_string()),
    );
    if !scope.is_empty() {
        claims.insert(
            "scope".to_string(),
            serde_json::Value::Array(
                scope
                    .iter()
                    .map(|item| serde_json::Value::String(item.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(exp) = expires_at {
        claims.insert("exp".to_string(), serde_json::Value::Number(exp.into()));
    }
    if let Some(kid) = kid {
        claims.insert("kid".to_string(), serde_json::Value::String(kid.to_string()));
    }

    let payload_bytes = serde_json::to_vec(&serde_json::Value::Object(claims))?;
    let signature = keypair.sign(&payload_bytes);
    Ok(format!(
        "PBX1.{}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload_bytes),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

pub fn expiry_from_date(date: &str) -> anyhow::Result<i64> {
    let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid date {date}: {e}"))?;
    let naive = parsed
        .and_hms_opt(23, 59, 59)
        .ok_or_else(|| anyhow::anyhow!("invalid date {date}"))?;
    Ok(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc).timestamp())
}

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

    let info = verify_license(&key_value, config.email.as_deref()).await?;

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

fn public_key_config_issue(
    public_key: Option<&str>,
    offline_tokens_present: bool,
) -> Option<String> {
    match public_key {
        Some(key) => {
            let valid = decode_hex(key)
                .map(|bytes| bytes.len() == 32)
                .unwrap_or(false);
            if valid {
                None
            } else {
                Some(
                    "configured [licenses].public_key is not a 32-byte hex key; offline PBX1 license tokens will be rejected"
                        .to_string(),
                )
            }
        }
        None if offline_tokens_present => Some(
            "offline PBX1 license tokens are configured but [licenses].public_key is missing; they will be rejected"
                .to_string(),
        ),
        None => None,
    }
}

pub async fn check_all_addon_licenses(
    addon_ids: &[String],
    license_config: &Option<LicenseConfig>,
) -> std::collections::HashMap<String, LicenseStatus> {
    let mut results = std::collections::HashMap::new();

    let enforce = license_config.as_ref().map(|c| c.enforce).unwrap_or(false);
    if !enforce {
        for id in addon_ids {
            results.insert(id.clone(), trial_status());
        }
        return results;
    }

    let config = license_config.as_ref().expect("enforce implies config");

    if let Some(issue) = public_key_config_issue(
        config.public_key.as_deref(),
        config
            .keys
            .values()
            .any(|value| value.starts_with(OFFLINE_TOKEN_PREFIX)),
    ) {
        tracing::error!("{}", issue);
    }

    for addon_id in addon_ids {
        let status = match config.addons.get(addon_id) {
            Some(key_name)
                if key_name == crate::config::GLOBAL_KEY_NAME && config.allow_global =>
            {
                LicenseStatus {
                    key_name: key_name.clone(),
                    valid: true,
                    expired: false,
                    expiry: None,
                    plan: "global".to_string(),
                    is_trial: true,
                    scope: None,
                }
            }
            Some(key_name) => match config.keys.get(key_name) {
                Some(key_value) => {
                    let verified = if key_value.starts_with(OFFLINE_TOKEN_PREFIX) {
                        match verify_offline(key_value, config.public_key.as_deref()) {
                            Some(info) => Ok(info),
                            None => Err(anyhow::anyhow!("offline license token rejected")),
                        }
                    } else {
                        verify_license(key_value, config.email.as_deref()).await
                    };
                    match verified {
                        Ok(info) => {
                            let expired = is_expired(&info);
                            let covered = info
                                .scope
                                .as_ref()
                                .map(|s| s.is_empty() || s.iter().any(|a| a == addon_id))
                                .unwrap_or(true);
                            LicenseStatus {
                                key_name: key_name.clone(),
                                valid: info.valid && !expired && covered,
                                expired,
                                expiry: info.expiry.map(|d| d.format("%Y-%m-%d").to_string()),
                                plan: info.plan,
                                is_trial: false,
                                scope: info.scope,
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to verify license for {}: {}", addon_id, e);
                            denied_status(key_name.clone())
                        }
                    }
                }
                None => denied_status(key_name.clone()),
            },
            None => denied_status(String::new()),
        };
        results.insert(addon_id.clone(), status);
    }

    results
}

pub async fn can_enable_addon(
    addon_id: &str,
    is_commercial: bool,
    license_config: &Option<LicenseConfig>,
) -> bool {
    if !is_commercial {
        return true;
    }
    let ids = vec![addon_id.to_string()];
    let mut results = check_all_addon_licenses(&ids, license_config).await;
    results.remove(addon_id).map(|s| s.valid).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn info(expiry: Option<chrono::DateTime<chrono::Utc>>) -> LicenseInfo {
        LicenseInfo {
            key: "test-key".to_string(),
            valid: true,
            expiry,
            plan: "pro".to_string(),
            last_checked: chrono::Utc::now(),
            scope: None,
            reject_reason: None,
        }
    }

    #[test]
    fn is_expired_false_for_future() {
        assert!(!is_expired(&info(Some(chrono::Utc::now() + Duration::days(30)))));
    }

    #[test]
    fn is_expired_true_for_past() {
        assert!(is_expired(&info(Some(chrono::Utc::now() - Duration::days(1)))));
    }

    #[test]
    fn is_expired_false_without_expiry() {
        assert!(!is_expired(&info(None)));
    }

    fn status_with(expiry: Option<&str>) -> LicenseStatus {
        LicenseStatus {
            key_name: "k".to_string(),
            valid: true,
            expired: false,
            expiry: expiry.map(|s| s.to_string()),
            plan: "pro".to_string(),
            is_trial: false,
            scope: None,
        }
    }

    fn date_offset(days: i64) -> String {
        (chrono::Utc::now().date_naive() + Duration::days(days))
            .format("%Y-%m-%d")
            .to_string()
    }

    #[test]
    fn days_until_expiry_none_without_expiry() {
        assert_eq!(status_with(None).days_until_expiry(), None);
        assert_eq!(status_with(Some("not-a-date")).days_until_expiry(), None);
    }

    #[test]
    fn days_until_expiry_positive_for_future() {
        let status = status_with(Some(&date_offset(10)));
        assert_eq!(status.days_until_expiry(), Some(10));
    }

    #[test]
    fn days_until_expiry_non_positive_for_past() {
        let status = status_with(Some(&date_offset(-5)));
        assert!(status.days_until_expiry().map(|d| d <= 0).unwrap_or(false));
    }

    #[test]
    fn expiring_soon_true_within_threshold() {
        let status = status_with(Some(&date_offset(10)));
        assert!(expiring_soon(&status, 30));
    }

    #[test]
    fn expiring_soon_false_outside_threshold() {
        let status = status_with(Some(&date_offset(60)));
        assert!(!expiring_soon(&status, 30));
    }

    #[test]
    fn expiring_soon_false_when_invalid_or_expired() {
        let mut invalid = status_with(Some(&date_offset(10)));
        invalid.valid = false;
        assert!(!expiring_soon(&invalid, 30));

        let mut expired = status_with(Some(&date_offset(-1)));
        expired.expired = true;
        expired.valid = false;
        assert!(!expiring_soon(&expired, 30));
    }

    #[test]
    fn expiring_soon_false_without_expiry() {
        assert!(!expiring_soon(&status_with(None), 30));
    }

    #[test]
    fn license_cache_ttl_is_positive() {
        assert!(LICENSE_CACHE_TTL_SECS > 0);
        assert!(chrono::Duration::seconds(LICENSE_CACHE_TTL_SECS) > chrono::Duration::zero());
    }

    fn insert_cached_entry(key: &str, last_checked: chrono::DateTime<chrono::Utc>) {
        let mut cached = info(None);
        cached.key = key.to_string();
        cached.plan = "cached-plan".to_string();
        cached.last_checked = last_checked;
        LICENSE_CACHE.insert(key.to_string(), cached);
    }

    #[test]
    fn cached_license_for_verify_serves_fresh_entry() {
        insert_cached_entry("ttl-test-fresh", chrono::Utc::now());
        let served = cached_license_for_verify("ttl-test-fresh").expect("fresh entry served");
        assert_eq!(served.plan, "cached-plan");
        assert!(get_cached_license("ttl-test-fresh").is_some());
        LICENSE_CACHE.remove("ttl-test-fresh");
    }

    #[test]
    fn cached_license_for_verify_rejects_backdated_entry() {
        insert_cached_entry(
            "ttl-test-backdated",
            chrono::Utc::now() - Duration::seconds(LICENSE_CACHE_TTL_SECS + 60),
        );
        assert!(get_cached_license("ttl-test-backdated").is_some());
        assert!(cached_license_for_verify("ttl-test-backdated").is_none());
        assert!(get_cached_license("ttl-test-backdated").is_none());
    }

    #[tokio::test]
    async fn verify_license_returns_fresh_cached_entry() {
        insert_cached_entry("ttl-test-verify", chrono::Utc::now());
        let served = verify_license("ttl-test-verify", None)
            .await
            .expect("served from cache");
        assert_eq!(served.plan, "cached-plan");
        LICENSE_CACHE.remove("ttl-test-verify");
    }

    #[tokio::test]
    async fn trial_when_no_config() {
        let ids = vec!["branding".to_string()];
        let results = check_all_addon_licenses(&ids, &None).await;
        let status = results.get("branding").expect("status recorded");
        assert!(status.valid);
        assert!(status.is_trial);
    }

    #[tokio::test]
    async fn not_enforced_allows_all() {
        let cfg = Some(LicenseConfig::default());
        let ids = vec!["branding".to_string(), "e911".to_string()];
        let results = check_all_addon_licenses(&ids, &cfg).await;
        assert_eq!(results.len(), 2);
        assert!(results.values().all(|s| s.valid && s.is_trial));
    }

    #[tokio::test]
    async fn enforced_without_key_denies() {
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        let results = check_all_addon_licenses(&vec!["branding".to_string()], &Some(cfg)).await;
        assert!(!results.get("branding").unwrap().valid);
    }

    #[tokio::test]
    async fn enforced_global_sentinel_allows() {
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        cfg.addons.insert("branding".to_string(), "global".to_string());
        let results = check_all_addon_licenses(&vec!["branding".to_string()], &Some(cfg)).await;
        let status = results.get("branding").unwrap();
        assert!(status.valid);
        assert!(status.is_trial);
    }

    #[tokio::test]
    async fn commercial_gate_uses_status() {
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        assert!(!can_enable_addon("e911", true, &Some(cfg)).await);
        assert!(can_enable_addon("queue", false, &None).await);
    }

    fn mint_token(payload: &serde_json::Value) -> (String, String) {
        use base64::Engine as _;
        use ring::signature::KeyPair as _;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pubkey_hex: String = kp
            .public_key()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let payload_bytes = serde_json::to_vec(payload).unwrap();
        let sig = kp.sign(&payload_bytes);
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let token = format!("PBX1.{}.{}", b64(&payload_bytes), b64(sig.as_ref()));
        (token, pubkey_hex)
    }

    #[test]
    fn offline_token_verifies() {
        let (token, pk) = mint_token(
            &serde_json::json!({"plan": "pro", "exp": 4102444800i64, "scope": ["branding"]}),
        );
        let info = verify_offline(&token, Some(&pk)).expect("valid token");
        assert_eq!(info.plan, "pro");
        assert!(info.expiry.is_some());
        assert_eq!(info.scope, Some(vec!["branding".to_string()]));
    }

    #[test]
    fn offline_token_rejects_tampered_payload() {
        use base64::Engine as _;
        let (token, pk) = mint_token(&serde_json::json!({"plan": "pro"}));
        let rest = token.strip_prefix("PBX1.").unwrap();
        let mut it = rest.split('.');
        let _payload = it.next().unwrap();
        let sig = it.next().unwrap();
        let fake = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"plan":"enterprise"}"#);
        let tampered = format!("PBX1.{fake}.{sig}");
        assert!(verify_offline(&tampered, Some(&pk)).is_none());
    }

    #[test]
    fn offline_token_rejects_wrong_key() {
        let (token, _pk) = mint_token(&serde_json::json!({"plan": "pro"}));
        let (_other, other_pk) = mint_token(&serde_json::json!({"plan": "pro"}));
        assert!(verify_offline(&token, Some(&other_pk)).is_none());
    }

    #[tokio::test]
    async fn enforced_offline_key_allows() {
        let (token, pk) = mint_token(&serde_json::json!({"plan": "pro", "scope": ["e911"]}));
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        cfg.public_key = Some(pk);
        cfg.addons.insert("e911".to_string(), "v1".to_string());
        cfg.keys.insert("v1".to_string(), token);
        let results = check_all_addon_licenses(&vec!["e911".to_string()], &Some(cfg)).await;
        let status = results.get("e911").unwrap();
        assert!(status.valid);
        assert!(!status.is_trial);
    }

    #[tokio::test]
    async fn enforced_offline_expired_denies() {
        let (token, pk) = mint_token(&serde_json::json!({"plan": "pro", "exp": 1}));
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        cfg.public_key = Some(pk);
        cfg.addons.insert("e911".to_string(), "v1".to_string());
        cfg.keys.insert("v1".to_string(), token);
        let results = check_all_addon_licenses(&vec!["e911".to_string()], &Some(cfg)).await;
        assert!(!results.get("e911").unwrap().valid);
    }

    #[test]
    fn keygen_mint_verify_round_trip() {
        let keypair = generate_keypair().expect("keypair");
        assert_eq!(keypair.public_key_hex.len(), 64);
        assert_eq!(
            public_key_from_private(&keypair.private_key_hex).expect("derive public"),
            keypair.public_key_hex
        );
        let scope = vec!["branding".to_string(), "fax".to_string()];
        let token = mint_offline_token(
            &keypair.private_key_hex,
            "pro",
            &scope,
            Some(4102444800),
            Some("acme"),
        )
        .expect("mint");
        let info = verify_offline(&token, Some(&keypair.public_key_hex)).expect("verify");
        assert!(info.valid);
        assert_eq!(info.plan, "pro");
        assert_eq!(info.scope, Some(scope));
        assert!(info.expiry.is_some());
    }

    #[test]
    fn minted_token_rejects_wrong_public_key() {
        let keypair = generate_keypair().expect("keypair");
        let other = generate_keypair().expect("keypair");
        let token =
            mint_offline_token(&keypair.private_key_hex, "pro", &[], None, None).expect("mint");
        assert!(verify_offline(&token, Some(&other.public_key_hex)).is_none());
    }

    #[test]
    fn mint_without_expiry_has_no_expiry() {
        let keypair = generate_keypair().expect("keypair");
        let token =
            mint_offline_token(&keypair.private_key_hex, "pro", &[], None, None).expect("mint");
        let info = verify_offline(&token, Some(&keypair.public_key_hex)).expect("verify");
        assert!(info.expiry.is_none());
    }

    #[test]
    fn expiry_from_date_parses_end_of_day() {
        let ts = expiry_from_date("2030-01-02").expect("parse");
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).expect("ts");
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2030-01-02");
        assert!(expiry_from_date("not-a-date").is_err());
    }

    #[test]
    fn mint_rejects_invalid_private_key() {
        assert!(mint_offline_token("zz", "pro", &[], None, None).is_err());
        assert!(public_key_from_private("").is_err());
    }

    #[test]
    fn public_key_config_issue_flags_invalid_key() {
        assert!(public_key_config_issue(None, false).is_none());
        assert!(public_key_config_issue(None, true).is_some());
        let valid = "a".repeat(64);
        assert!(public_key_config_issue(Some(&valid), false).is_none());
        let short = "a".repeat(62);
        let issue = public_key_config_issue(Some(&short), false).expect("issue");
        assert!(issue.contains("public_key"));
        let issue = public_key_config_issue(Some("zz"), false).expect("issue");
        assert!(issue.contains("hex"));
    }

    #[test]
    fn allow_global_defaults_to_enabled() {
        let parsed: LicenseConfig = serde_json::from_str("{}").expect("parse");
        assert!(parsed.allow_global);
        let parsed: LicenseConfig =
            serde_json::from_str(r#"{"allow_global": false}"#).expect("parse");
        assert!(!parsed.allow_global);
        assert!(LicenseConfig::default().allow_global);
    }

    #[tokio::test]
    async fn global_sentinel_denied_when_allow_global_disabled() {
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        cfg.allow_global = false;
        cfg.addons
            .insert("branding".to_string(), crate::config::GLOBAL_KEY_NAME.to_string());
        let results = check_all_addon_licenses(&vec!["branding".to_string()], &Some(cfg)).await;
        let status = results.get("branding").unwrap();
        assert!(!status.valid);
        assert!(!status.is_trial);
    }

    #[tokio::test]
    async fn global_sentinel_trial_remains_the_default() {
        let mut cfg = LicenseConfig::default();
        cfg.enforce = true;
        cfg.addons
            .insert("branding".to_string(), crate::config::GLOBAL_KEY_NAME.to_string());
        let results = check_all_addon_licenses(&vec!["branding".to_string()], &Some(cfg)).await;
        let status = results.get("branding").unwrap();
        assert!(status.valid);
        assert!(status.is_trial);
    }
}
