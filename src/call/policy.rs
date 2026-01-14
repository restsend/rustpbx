use crate::models::policy::PolicySpec;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

#[async_trait]
pub trait FrequencyLimiter: Send + Sync {
    async fn check_and_increment(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        limit: u32,
        window_hours: u32,
    ) -> Result<bool>;

    async fn check_daily_limit(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        limit: u32,
    ) -> Result<bool>;

    async fn check_concurrency(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        max_concurrency: u32,
    ) -> Result<bool>;

    async fn release_concurrency(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
    ) -> Result<()>;

    async fn list_limits(
        &self,
        policy_id: Option<String>,
        scope: Option<String>,
        scope_value: Option<String>,
        limit_type: Option<String>,
    ) -> Result<Vec<crate::models::frequency_limit::Model>>;

    async fn clear_limits(
        &self,
        policy_id: Option<String>,
        scope: Option<String>,
        scope_value: Option<String>,
        limit_type: Option<String>,
    ) -> Result<u64>;
}

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum PolicyRejection {
    DestinationNotAllowed(String),
    LandlineNotAllowed(String),
    TimeWindowDenied(String),
    RegionDenied(String),
    FrequencyLimitExceeded(String),
    DailyLimitExceeded(String),
    ConcurrencyLimitExceeded(String),
    InvalidNumber(String),
}

impl fmt::Display for PolicyRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyRejection::DestinationNotAllowed(msg) => write!(f, "{}", msg),
            PolicyRejection::LandlineNotAllowed(msg) => write!(f, "{}", msg),
            PolicyRejection::TimeWindowDenied(msg) => write!(f, "{}", msg),
            PolicyRejection::RegionDenied(msg) => write!(f, "{}", msg),
            PolicyRejection::FrequencyLimitExceeded(msg) => write!(f, "{}", msg),
            PolicyRejection::DailyLimitExceeded(msg) => write!(f, "{}", msg),
            PolicyRejection::ConcurrencyLimitExceeded(msg) => write!(f, "{}", msg),
            PolicyRejection::InvalidNumber(msg) => write!(f, "{}", msg),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PolicyCheckStatus {
    Allowed,
    Rejected(PolicyRejection),
}

pub struct PolicyGuard {
    limiter: Arc<dyn FrequencyLimiter>,
}

impl fmt::Debug for PolicyGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyGuard").finish()
    }
}

impl PolicyGuard {
    pub fn new(limiter: Arc<dyn FrequencyLimiter>) -> Self {
        Self { limiter }
    }

    pub async fn check_policy(
        &self,
        policy: &PolicySpec,
        caller: &str,
        callee: &str,
        origin_country: Option<&str>,
    ) -> Result<PolicyCheckStatus> {
        // 1. Static Checks
        if let Some(prefix) = &policy.called_prefix {
            if !callee.starts_with(prefix) {
                // If prefix doesn't match, does the policy apply?
                // Usually policy applies if conditions match.
                // If this is a restriction, maybe we skip this policy?
                // Assuming this is a filter for the policy.
                return Ok(PolicyCheckStatus::Allowed);
            }
        }

        if let Some(tc) = &policy.trunk_country {
            if let Some(oc) = origin_country {
                if tc != oc {
                    // Policy for different country
                    return Ok(PolicyCheckStatus::Allowed);
                }
            }
        }

        // Parse the callee number to get country and type
        let parsed_number = phonenumber::parse(None, callee).ok();

        if !policy.allowed_destination_countries.is_empty() {
            if let Some(ref number) = parsed_number {
                if let Some(id) = number.country().id() {
                    let region_str = id.as_ref();
                    if !policy
                        .allowed_destination_countries
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case(region_str))
                    {
                        let reason = format!(
                            "Destination country {} not allowed for callee {}",
                            region_str, callee
                        );
                        warn!("{}", reason);
                        return Ok(PolicyCheckStatus::Rejected(
                            PolicyRejection::DestinationNotAllowed(reason),
                        ));
                    }
                } else {
                    let reason = format!("No country ID found for callee {}", callee);
                    warn!("{}", reason);
                    return Ok(PolicyCheckStatus::Rejected(PolicyRejection::InvalidNumber(
                        reason,
                    )));
                }
            }
        }

        if let Some(allow_landline) = policy.allow_landline {
            if let Some(ref number) = parsed_number {
                let num_type = number.number_type(&phonenumber::metadata::DATABASE);
                let is_fixed = matches!(
                    num_type,
                    phonenumber::Type::FixedLine | phonenumber::Type::FixedLineOrMobile
                );

                if !allow_landline && is_fixed {
                    let reason = format!("Landline calls are not allowed: {}", callee);
                    warn!("{}", reason);
                    return Ok(PolicyCheckStatus::Rejected(
                        PolicyRejection::LandlineNotAllowed(reason),
                    ));
                }
            }
        }

        if let Some(window) = &policy.time_window {
            use chrono::NaiveTime;
            use chrono_tz::Tz;
            use std::str::FromStr;

            let start = NaiveTime::parse_from_str(&window.start, "%H:%M")
                .map_err(|e| anyhow::anyhow!("Invalid start time: {}", e))?;
            let end = NaiveTime::parse_from_str(&window.end, "%H:%M")
                .map_err(|e| anyhow::anyhow!("Invalid end time: {}", e))?;

            let now_time = if window.timezone == "Local" {
                Local::now().time()
            } else {
                let tz = Tz::from_str(&window.timezone)
                    .map_err(|e| anyhow::anyhow!("Invalid timezone: {}", e))?;
                Utc::now().with_timezone(&tz).time()
            };

            if start <= end {
                if now_time < start || now_time > end {
                    let reason = format!(
                        "Call outside allowed time window: {} - {}",
                        window.start, window.end
                    );
                    warn!("{}", reason);
                    return Ok(PolicyCheckStatus::Rejected(
                        PolicyRejection::TimeWindowDenied(reason),
                    ));
                }
            } else {
                // Window crosses midnight (e.g. 22:00 - 06:00)
                if now_time < start && now_time > end {
                    let reason = format!(
                        "Call outside allowed time window: {} - {}",
                        window.start, window.end
                    );
                    warn!("{}", reason);
                    return Ok(PolicyCheckStatus::Rejected(
                        PolicyRejection::TimeWindowDenied(reason),
                    ));
                }
            }
        }

        if !policy.deny_regions.is_empty() {
            // Check Callee Region
            if let Some(ref number) = parsed_number {
                if let Some(id) = number.country().id() {
                    let region_str = id.as_ref();
                    if policy
                        .deny_regions
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(region_str))
                    {
                        let reason = format!(
                            "Destination country {} is denied for callee {}",
                            region_str, callee
                        );
                        warn!("{}", reason);
                        return Ok(PolicyCheckStatus::Rejected(PolicyRejection::RegionDenied(
                            reason,
                        )));
                    }
                }
            }

            // Check Caller Region
            let parsed_caller = phonenumber::parse(None, caller).ok();
            if let Some(ref number) = parsed_caller {
                if let Some(id) = number.country().id() {
                    let region_str = id.as_ref();
                    if policy
                        .deny_regions
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(region_str))
                    {
                        let reason = format!(
                            "Source country {} is denied for caller {}",
                            region_str, caller
                        );
                        warn!("{}", reason);
                        return Ok(PolicyCheckStatus::Rejected(PolicyRejection::RegionDenied(
                            reason,
                        )));
                    }
                }
            }

            // Check Origin Country
            if let Some(oc) = origin_country {
                if policy
                    .deny_regions
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(oc))
                {
                    let reason = format!("Origin country {} is denied", oc);
                    warn!("{}", reason);
                    return Ok(PolicyCheckStatus::Rejected(PolicyRejection::RegionDenied(
                        reason,
                    )));
                }
            }
        }

        // 2. Dynamic Checks (Frequency Limit)
        if let Some(limit) = &policy.frequency_limit {
            if !self
                .limiter
                .check_and_increment(
                    "policy_id", // TODO: Pass policy ID
                    "caller",
                    caller,
                    limit.count,
                    limit.window_hours,
                )
                .await?
            {
                let reason = format!("Frequency limit reached for caller {}", caller);
                warn!("{}", reason);
                return Ok(PolicyCheckStatus::Rejected(
                    PolicyRejection::FrequencyLimitExceeded(reason),
                ));
            }
        }

        if let Some(limit) = &policy.daily_limit {
            if !self
                .limiter
                .check_daily_limit("policy_id", "caller", caller, limit.count)
                .await?
            {
                let reason = format!("Daily limit reached for caller {}", caller);
                warn!("{}", reason);
                return Ok(PolicyCheckStatus::Rejected(
                    PolicyRejection::DailyLimitExceeded(reason),
                ));
            }
        }

        if let Some(limit) = &policy.concurrency {
            if !self
                .limiter
                .check_concurrency("policy_id", "caller", caller, limit.max_total)
                .await?
            {
                let reason = format!("Concurrency limit reached for caller {}", caller);
                warn!("{}", reason);
                return Ok(PolicyCheckStatus::Rejected(
                    PolicyRejection::ConcurrencyLimitExceeded(reason),
                ));
            }
        }

        Ok(PolicyCheckStatus::Allowed)
    }
}

// Mock implementation for now
pub struct MockFrequencyLimiter;

#[async_trait]
impl FrequencyLimiter for MockFrequencyLimiter {
    async fn check_and_increment(
        &self,
        _policy_id: &str,
        _scope: &str,
        _scope_value: &str,
        _limit: u32,
        _window_hours: u32,
    ) -> Result<bool> {
        // Always allow in mock
        Ok(true)
    }

    async fn check_daily_limit(
        &self,
        _policy_id: &str,
        _scope: &str,
        _scope_value: &str,
        _limit: u32,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn check_concurrency(
        &self,
        _policy_id: &str,
        _scope: &str,
        _scope_value: &str,
        _max_concurrency: u32,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn release_concurrency(
        &self,
        _policy_id: &str,
        _scope: &str,
        _scope_value: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn list_limits(
        &self,
        _policy_id: Option<String>,
        _scope: Option<String>,
        _scope_value: Option<String>,
        _limit_type: Option<String>,
    ) -> Result<Vec<crate::models::frequency_limit::Model>> {
        Ok(vec![])
    }

    async fn clear_limits(
        &self,
        _policy_id: Option<String>,
        _scope: Option<String>,
        _scope_value: Option<String>,
        _limit_type: Option<String>,
    ) -> Result<u64> {
        Ok(0)
    }
}

pub struct InMemoryFrequencyLimiter {
    // Key: "policy_id:scope:scope_value:type" -> (count, window_end_timestamp)
    // For concurrency, window_end is i64::MAX
    counts: RwLock<HashMap<String, (u32, i64)>>,
}

impl InMemoryFrequencyLimiter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            counts: RwLock::new(HashMap::new()),
        })
    }
    pub async fn run_cleanup_loop(self: Arc<Self>, cancel_token: CancellationToken) {
        let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.cleanup();
                }
                _ = cancel_token.cancelled() => {
                    break;
                }
            }
        }
    }

    pub fn start_cleanup(self: Arc<Self>, cancel_token: CancellationToken) {
        tokio::spawn(async move {
            self.run_cleanup_loop(cancel_token).await;
        });
    }

    fn cleanup(&self) {
        let now = Utc::now().timestamp();
        let mut counts = self.counts.write().unwrap();
        // Only cleanup expired windows. Concurrency (MAX) won't be cleaned up here unless we want to.
        // Concurrency should be released explicitly.
        counts.retain(|_, (_, window_end)| *window_end > now);
    }

    fn get_key(&self, policy_id: &str, scope: &str, scope_value: &str, limit_type: &str) -> String {
        format!("{}:{}:{}:{}", policy_id, scope, scope_value, limit_type)
    }
}

#[async_trait]
impl FrequencyLimiter for InMemoryFrequencyLimiter {
    async fn check_and_increment(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        limit: u32,
        window_hours: u32,
    ) -> Result<bool> {
        let key = self.get_key(policy_id, scope, scope_value, "frequency");
        let now = Utc::now().timestamp();
        let mut counts = self.counts.write().unwrap();

        let (count, window_end) = counts.entry(key.clone()).or_insert((0, 0));

        if now > *window_end {
            // Window expired, reset
            *count = 1;
            *window_end = now + (window_hours as i64 * 3600);
            Ok(true)
        } else {
            if *count >= limit {
                Ok(false)
            } else {
                *count += 1;
                Ok(true)
            }
        }
    }

    async fn check_daily_limit(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        limit: u32,
    ) -> Result<bool> {
        let key = self.get_key(policy_id, scope, scope_value, "daily");
        let now = Utc::now();
        let today_end = now
            .date_naive()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_local_timezone(Utc)
            .unwrap()
            .timestamp();

        let mut counts = self.counts.write().unwrap();
        let (count, window_end) = counts.entry(key.clone()).or_insert((0, 0));

        if now.timestamp() > *window_end {
            // New day (or first time)
            *count = 1;
            *window_end = today_end;
            Ok(true)
        } else {
            if *count >= limit {
                Ok(false)
            } else {
                *count += 1;
                Ok(true)
            }
        }
    }

    async fn check_concurrency(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        max_concurrency: u32,
    ) -> Result<bool> {
        let key = self.get_key(policy_id, scope, scope_value, "concurrency");
        let mut counts = self.counts.write().unwrap();
        let (count, window_end) = counts.entry(key.clone()).or_insert((0, i64::MAX));

        // Ensure it's marked as concurrency type (MAX window)
        *window_end = i64::MAX;

        if *count >= max_concurrency {
            Ok(false)
        } else {
            *count += 1;
            Ok(true)
        }
    }

    async fn release_concurrency(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
    ) -> Result<()> {
        let key = self.get_key(policy_id, scope, scope_value, "concurrency");
        let mut counts = self.counts.write().unwrap();
        if let Some((count, _)) = counts.get_mut(&key) {
            if *count > 0 {
                *count -= 1;
            }
        }
        Ok(())
    }

    async fn list_limits(
        &self,
        policy_id: Option<String>,
        scope: Option<String>,
        scope_value: Option<String>,
        limit_type: Option<String>,
    ) -> Result<Vec<crate::models::frequency_limit::Model>> {
        let counts = self.counts.read().unwrap();
        let mut results = Vec::new();
        for (key, (count, window_end)) in counts.iter() {
            let parts: Vec<&str> = key.split(':').collect();
            if parts.len() != 4 {
                continue;
            }
            let p_id = parts[0];
            let s = parts[1];
            let s_val = parts[2];
            let l_type = parts[3];

            if let Some(ref pid) = policy_id {
                if pid != p_id {
                    continue;
                }
            }
            if let Some(ref sc) = scope {
                if sc != s {
                    continue;
                }
            }
            if let Some(ref sv) = scope_value {
                if sv != s_val {
                    continue;
                }
            }
            if let Some(ref lt) = limit_type {
                if lt != l_type {
                    continue;
                }
            }

            results.push(crate::models::frequency_limit::Model {
                id: 0, // Dummy ID
                policy_id: p_id.to_string(),
                scope: s.to_string(),
                scope_value: s_val.to_string(),
                limit_type: l_type.to_string(),
                count: *count,
                window_end: if *window_end == i64::MAX {
                    None
                } else {
                    Some(chrono::DateTime::from_timestamp(*window_end, 0).unwrap_or_default())
                },
                updated_at: Utc::now(),
            });
        }
        Ok(results)
    }

    async fn clear_limits(
        &self,
        policy_id: Option<String>,
        scope: Option<String>,
        scope_value: Option<String>,
        limit_type: Option<String>,
    ) -> Result<u64> {
        let mut counts = self.counts.write().unwrap();
        let initial_len = counts.len();
        counts.retain(|key, _| {
            let parts: Vec<&str> = key.split(':').collect();
            if parts.len() != 4 {
                return true;
            }
            let p_id = parts[0];
            let s = parts[1];
            let s_val = parts[2];
            let l_type = parts[3];

            if let Some(ref pid) = policy_id {
                if pid != p_id {
                    return true;
                }
            }
            if let Some(ref sc) = scope {
                if sc != s {
                    return true;
                }
            }
            if let Some(ref sv) = scope_value {
                if sv != s_val {
                    return true;
                }
            }
            if let Some(ref lt) = limit_type {
                if lt != l_type {
                    return true;
                }
            }
            false // Remove
        });
        Ok((initial_len - counts.len()) as u64)
    }
}

pub struct DbFrequencyLimiter {
    db: DatabaseConnection,
}

impl DbFrequencyLimiter {
    pub fn new(db: DatabaseConnection) -> Arc<Self> {
        Arc::new(Self { db })
    }

    pub async fn run_cleanup_loop(self: Arc<Self>, cancel_token: CancellationToken) {
        let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.cleanup().await {
                        error!("Error during frequency limiter cleanup: {}", e);
                    }
                }
                _ = cancel_token.cancelled() => {
                    break;
                }
            }
        }
    }

    pub fn start_cleanup(self: Arc<Self>, cancel_token: CancellationToken) {
        tokio::spawn(async move {
            self.run_cleanup_loop(cancel_token).await;
        });
    }

    async fn cleanup(&self) -> Result<()> {
        use crate::models::frequency_limit;
        frequency_limit::Entity::delete_many()
            .filter(frequency_limit::Column::WindowEnd.lt(Utc::now()))
            .exec(&self.db)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl FrequencyLimiter for DbFrequencyLimiter {
    async fn check_and_increment(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        limit: u32,
        window_hours: u32,
    ) -> Result<bool> {
        use crate::models::frequency_limit;
        let now = Utc::now();
        let policy_id = policy_id.to_string();
        let scope = scope.to_string();
        let scope_value = scope_value.to_string();

        let result = self
            .db
            .transaction::<_, bool, anyhow::Error>(|txn| {
                Box::pin(async move {
                    let record = frequency_limit::Entity::find()
                        .filter(frequency_limit::Column::PolicyId.eq(&policy_id))
                        .filter(frequency_limit::Column::Scope.eq(&scope))
                        .filter(frequency_limit::Column::ScopeValue.eq(&scope_value))
                        .filter(frequency_limit::Column::LimitType.eq("frequency"))
                        .one(txn)
                        .await?;

                    if let Some(model) = record {
                        if let Some(window_end) = model.window_end {
                            if now > window_end {
                                let mut active: frequency_limit::ActiveModel = model.into();
                                active.count = Set(1);
                                active.window_end =
                                    Set(Some(now + chrono::Duration::hours(window_hours as i64)));
                                active.updated_at = Set(now);
                                active.update(txn).await?;
                                Ok(true)
                            } else {
                                if model.count >= limit {
                                    Ok(false)
                                } else {
                                    let count = model.count;
                                    let mut active: frequency_limit::ActiveModel = model.into();
                                    active.count = Set(count + 1);
                                    active.updated_at = Set(now);
                                    active.update(txn).await?;
                                    Ok(true)
                                }
                            }
                        } else {
                            let mut active: frequency_limit::ActiveModel = model.into();
                            active.count = Set(1);
                            active.window_end =
                                Set(Some(now + chrono::Duration::hours(window_hours as i64)));
                            active.updated_at = Set(now);
                            active.update(txn).await?;
                            Ok(true)
                        }
                    } else {
                        let active = frequency_limit::ActiveModel {
                            policy_id: Set(policy_id),
                            scope: Set(scope),
                            scope_value: Set(scope_value),
                            limit_type: Set("frequency".to_string()),
                            count: Set(1),
                            window_end: Set(Some(
                                now + chrono::Duration::hours(window_hours as i64),
                            )),
                            updated_at: Set(now),
                            ..Default::default()
                        };
                        active.insert(txn).await?;
                        Ok(true)
                    }
                })
            })
            .await?;

        Ok(result)
    }

    async fn check_daily_limit(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        limit: u32,
    ) -> Result<bool> {
        use crate::models::frequency_limit;
        let now = Utc::now();
        let today_end = now
            .date_naive()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_local_timezone(Utc)
            .unwrap();

        let record = frequency_limit::Entity::find()
            .filter(frequency_limit::Column::PolicyId.eq(policy_id))
            .filter(frequency_limit::Column::Scope.eq(scope))
            .filter(frequency_limit::Column::ScopeValue.eq(scope_value))
            .filter(frequency_limit::Column::LimitType.eq("daily"))
            .one(&self.db)
            .await?;

        if let Some(model) = record {
            if let Some(window_end) = model.window_end {
                if now > window_end {
                    let mut active: frequency_limit::ActiveModel = model.into();
                    active.count = Set(1);
                    active.window_end = Set(Some(today_end));
                    active.updated_at = Set(now);
                    active.update(&self.db).await?;
                    Ok(true)
                } else {
                    if model.count >= limit {
                        Ok(false)
                    } else {
                        let count = model.count;
                        let mut active: frequency_limit::ActiveModel = model.into();
                        active.count = Set(count + 1);
                        active.updated_at = Set(now);
                        active.update(&self.db).await?;
                        Ok(true)
                    }
                }
            } else {
                let mut active: frequency_limit::ActiveModel = model.into();
                active.count = Set(1);
                active.window_end = Set(Some(today_end));
                active.updated_at = Set(now);
                active.update(&self.db).await?;
                Ok(true)
            }
        } else {
            let active = frequency_limit::ActiveModel {
                policy_id: Set(policy_id.to_string()),
                scope: Set(scope.to_string()),
                scope_value: Set(scope_value.to_string()),
                limit_type: Set("daily".to_string()),
                count: Set(1),
                window_end: Set(Some(today_end)),
                updated_at: Set(now),
                ..Default::default()
            };
            active.insert(&self.db).await?;
            Ok(true)
        }
    }

    async fn check_concurrency(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
        max_concurrency: u32,
    ) -> Result<bool> {
        use crate::models::frequency_limit;
        let now = Utc::now();

        let record = frequency_limit::Entity::find()
            .filter(frequency_limit::Column::PolicyId.eq(policy_id))
            .filter(frequency_limit::Column::Scope.eq(scope))
            .filter(frequency_limit::Column::ScopeValue.eq(scope_value))
            .filter(frequency_limit::Column::LimitType.eq("concurrency"))
            .one(&self.db)
            .await?;

        if let Some(model) = record {
            if model.count >= max_concurrency {
                Ok(false)
            } else {
                let count = model.count;
                let mut active: frequency_limit::ActiveModel = model.into();
                active.count = Set(count + 1);
                active.updated_at = Set(now);
                active.update(&self.db).await?;
                Ok(true)
            }
        } else {
            let active = frequency_limit::ActiveModel {
                policy_id: Set(policy_id.to_string()),
                scope: Set(scope.to_string()),
                scope_value: Set(scope_value.to_string()),
                limit_type: Set("concurrency".to_string()),
                count: Set(1),
                window_end: Set(None), // No window for concurrency
                updated_at: Set(now),
                ..Default::default()
            };
            active.insert(&self.db).await?;
            Ok(true)
        }
    }

    async fn release_concurrency(
        &self,
        policy_id: &str,
        scope: &str,
        scope_value: &str,
    ) -> Result<()> {
        use crate::models::frequency_limit;
        let now = Utc::now();

        let record = frequency_limit::Entity::find()
            .filter(frequency_limit::Column::PolicyId.eq(policy_id))
            .filter(frequency_limit::Column::Scope.eq(scope))
            .filter(frequency_limit::Column::ScopeValue.eq(scope_value))
            .filter(frequency_limit::Column::LimitType.eq("concurrency"))
            .one(&self.db)
            .await?;

        if let Some(model) = record {
            if model.count > 0 {
                let count = model.count;
                let mut active: frequency_limit::ActiveModel = model.into();
                active.count = Set(count - 1);
                active.updated_at = Set(now);
                active.update(&self.db).await?;
            }
        }
        Ok(())
    }

    async fn list_limits(
        &self,
        policy_id: Option<String>,
        scope: Option<String>,
        scope_value: Option<String>,
        limit_type: Option<String>,
    ) -> Result<Vec<crate::models::frequency_limit::Model>> {
        use crate::models::frequency_limit;
        let mut query = frequency_limit::Entity::find();

        if let Some(policy_id) = policy_id {
            query = query.filter(frequency_limit::Column::PolicyId.eq(policy_id));
        }
        if let Some(scope) = scope {
            query = query.filter(frequency_limit::Column::Scope.eq(scope));
        }
        if let Some(scope_value) = scope_value {
            query = query.filter(frequency_limit::Column::ScopeValue.eq(scope_value));
        }
        if let Some(limit_type) = limit_type {
            query = query.filter(frequency_limit::Column::LimitType.eq(limit_type));
        }

        Ok(query.all(&self.db).await?)
    }

    async fn clear_limits(
        &self,
        policy_id: Option<String>,
        scope: Option<String>,
        scope_value: Option<String>,
        limit_type: Option<String>,
    ) -> Result<u64> {
        use crate::models::frequency_limit;
        let mut query = frequency_limit::Entity::delete_many();

        if let Some(policy_id) = policy_id {
            query = query.filter(frequency_limit::Column::PolicyId.eq(policy_id));
        }
        if let Some(scope) = scope {
            query = query.filter(frequency_limit::Column::Scope.eq(scope));
        }
        if let Some(scope_value) = scope_value {
            query = query.filter(frequency_limit::Column::ScopeValue.eq(scope_value));
        }
        if let Some(limit_type) = limit_type {
            query = query.filter(frequency_limit::Column::LimitType.eq(limit_type));
        }

        let res = query.exec(&self.db).await?;
        Ok(res.rows_affected)
    }
}
