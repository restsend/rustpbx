use arc_swap::ArcSwap;
use chrono::Utc;
use crate::config::BanConfig;
use rustpbx_models::security_ban::{self, Entity as Bans, Model as BanRecord};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type NowFn = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Debug, Clone)]
pub struct AlertsConfig {
    pub webhook_url: Option<String>,
}

pub struct BanStore {
    db: DatabaseConnection,
    config: BanConfig,
    webhook_url: Option<String>,
    mail: Option<(Arc<dyn crate::mail::MailTransport>, Vec<String>)>,
    branding: parking_lot::RwLock<Option<Arc<dyn crate::branding::BrandingProvider>>>,
    http: reqwest::Client,
    banned: ArcSwap<HashMap<IpAddr, Instant>>,
    failures: parking_lot::Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    now: NowFn,
}

impl BanStore {
    pub async fn load(
        db: DatabaseConnection,
        config: BanConfig,
        webhook_url: Option<String>,
        mail: Option<(Arc<dyn crate::mail::MailTransport>, Vec<String>)>,
    ) -> anyhow::Result<Arc<Self>> {
        let store = Self {
            db,
            config,
            webhook_url,
            mail,
            branding: parking_lot::RwLock::new(None),
            http: reqwest::Client::new(),
            banned: ArcSwap::from_pointee(HashMap::new()),
            failures: parking_lot::Mutex::new(HashMap::new()),
            now: Arc::new(Instant::now),
        };
        store.reload_from_db().await?;
        Ok(Arc::new(store))
    }

    pub fn with_clock(
        db: DatabaseConnection,
        config: BanConfig,
        webhook_url: Option<String>,
        now: NowFn,
    ) -> Self {
        Self {
            db,
            config,
            webhook_url,
            mail: None,
            branding: parking_lot::RwLock::new(None),
            http: reqwest::Client::new(),
            banned: ArcSwap::from_pointee(HashMap::new()),
            failures: parking_lot::Mutex::new(HashMap::new()),
            now,
        }
    }

    pub fn with_branding(
        self: Arc<Self>,
        provider: Option<Arc<dyn crate::branding::BrandingProvider>>,
    ) -> Arc<Self> {
        *self.branding.write() = provider;
        self
    }

    pub fn with_mail(
        mut self,
        transport: Arc<dyn crate::mail::MailTransport>,
        recipients: Vec<String>,
    ) -> Self {
        self.mail = Some((transport, recipients));
        self
    }

    pub async fn reload(&self) -> anyhow::Result<()> {
        self.reload_from_db().await
    }

    async fn reload_from_db(&self) -> anyhow::Result<()> {
        let now_ts = Utc::now().timestamp();
        let active = Bans::find()
            .filter(security_ban::Column::BannedUntil.gt(now_ts))
            .filter(security_ban::Column::ReleasedAt.is_null())
            .all(&self.db)
            .await?;
        let mut map = HashMap::new();
        for record in active {
            if let Ok(ip) = record.ip.parse::<IpAddr>() {
                let until_instant = self.instant_from_unix(record.banned_until);
                map.insert(ip, until_instant);
            }
        }
        self.banned.store(Arc::new(map));
        Ok(())
    }

    fn instant_from_unix(&self, ts: i64) -> Instant {
        let now_unix = Utc::now().timestamp();
        let delta = (ts - now_unix).max(0) as u64;
        (self.now)() + Duration::from_secs(delta)
    }

    pub fn is_banned(&self, ip: &IpAddr) -> bool {
        let map = self.banned.load();
        match map.get(ip) {
            Some(until) => (self.now)() < *until,
            None => false,
        }
    }

    fn is_protected(&self, ip: &IpAddr) -> bool {
        self.config.protected_cidrs.iter().any(|cidr| {
            let Some((net, bits)) = cidr.split_once('/') else {
                return false;
            };
            let Ok(net_ip) = net.parse::<IpAddr>() else {
                return false;
            };
            let Ok(bits) = bits.parse::<u32>() else {
                return false;
            };
            match (net_ip, ip) {
                (IpAddr::V4(net), IpAddr::V4(peer)) => {
                    let net_u = u32::from(net);
                    let peer_u = u32::from(*peer);
                    bits > 0
                        && bits <= 32
                        && (net_u >> (32 - bits)) == (peer_u >> (32 - bits))
                }
                _ => false,
            }
        })
    }

    pub async fn record_auth_failure(&self, ip: IpAddr, username: &str, method: &str) {
        if !self.config.enabled || self.is_protected(&ip) {
            return;
        }
        let now = (self.now)();
        let window = Duration::from_secs(self.config.window_secs);
        let (count, reached) = {
            let mut failures = self.failures.lock();
            let queue = failures.entry(ip).or_default();
            while let Some(front) = queue.front() {
                if now.duration_since(*front) > window {
                    queue.pop_front();
                } else {
                    break;
                }
            }
            queue.push_back(now);
            let count = queue.len() as u32;
            let reached = count >= self.config.max_failures;
            if reached {
                failures.remove(&ip);
            }
            (count, reached)
        };
        if reached {
            if let Err(e) = self.apply_ban(ip, username, method).await {
                tracing::error!(%ip, error = %e, "failed to persist ban");
            }
        } else {
            tracing::warn!(
                %ip,
                count,
                threshold = self.config.max_failures,
                username,
                "auth failure recorded"
            );
        }
    }

    async fn apply_ban(&self, ip: IpAddr, username: &str, method: &str) -> anyhow::Result<()> {
        let now_ts = Utc::now().timestamp();
        let prior = Bans::find_by_id(ip.to_string()).one(&self.db).await?;
        let (offense_count, created_at) = match &prior {
            Some(existing) => (existing.offense_count + 1, existing.created_at),
            None => (1, now_ts),
        };
        let durations = if self.config.ban_durations_secs.is_empty() {
            vec![3600u64, 86400, 604800]
        } else {
            self.config.ban_durations_secs.clone()
        };
        let index = ((offense_count - 1) as usize).min(durations.len() - 1);
        let duration = durations[index];
        let until_ts = now_ts + duration as i64;

        let active_model = security_ban::ActiveModel {
            ip: Set(ip.to_string()),
            reason: Set("auth_failures".to_string()),
            last_username: Set(Some(username.to_string())),
            last_method: Set(Some(method.to_string())),
            offense_count: Set(offense_count),
            banned_until: Set(until_ts),
            created_at: Set(created_at),
            released_at: Set(None),
            released_by: Set(None),
        };
        Bans::insert(active_model)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(security_ban::Column::Ip)
                    .update_columns([
                        security_ban::Column::Reason,
                        security_ban::Column::LastUsername,
                        security_ban::Column::LastMethod,
                        security_ban::Column::OffenseCount,
                        security_ban::Column::BannedUntil,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await?;

        let until_instant = self.instant_from_unix(until_ts);
        self.banned.rcu(|current| {
            let mut map = HashMap::clone(current);
            map.insert(ip, until_instant);
            map
        });

        tracing::warn!(
            %ip,
            offense_count,
            ban_hours = duration / 3600,
            username,
            "source banned after repeated auth failures"
        );
        self.emit_alert(serde_json::json!({
            "type": "auth_ban",
            "ip": ip.to_string(),
            "username": username,
            "method": method,
            "offense_count": offense_count,
            "banned_until": until_ts,
        }));
        Ok(())
    }

    fn emit_alert(&self, payload: serde_json::Value) {
        self.emit_webhook_alert(&payload);
        self.emit_mail_alert(&payload);
    }

    fn emit_mail_alert(&self, payload: &serde_json::Value) {
        let Some((transport, recipients)) = self.mail.clone() else {
            return;
        };
        let alert_type = payload
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("alert");
        let ip = payload
            .get("ip")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let default_subject = format!("rustpbx {}: {}", alert_type, ip);
        let default_body = serde_json::to_string_pretty(payload).unwrap_or_default();
        let branding = self.branding.read().clone();
        let (subject, body) = match branding.as_ref().and_then(|p| p.email_template()) {
            Some(template) => {
                let vars = [
                    ("alert_type", alert_type),
                    ("ip", ip),
                    ("payload", default_body.as_str()),
                ];
                let rendered_subject = crate::mail::render_template(&template.subject, &vars);
                let rendered_body = crate::mail::render_template(&template.body, &vars);
                let subject = if rendered_subject.is_empty() {
                    default_subject.clone()
                } else {
                    rendered_subject
                };
                let body = if rendered_body.is_empty() {
                    default_body.clone()
                } else {
                    rendered_body
                };
                (subject, body)
            }
            None => (default_subject.clone(), default_body.clone()),
        };
        crate::utils::spawn(async move {
            for delay_secs in [1u64, 5u64, 15u64] {
                let mut mail = crate::mail::OutboundMail {
                    to: recipients.clone(),
                    subject: subject.clone(),
                    body: body.clone(),
                    from_name: None,
                    footer: None,
                    attachments: Vec::new(),
                };
                crate::mail::apply_brand(&mut mail, branding.as_ref());
                match transport.send(&mail).await {
                    Ok(()) => return,
                    Err(e) => {
                        tracing::warn!(error = %e, "alert mail send failed");
                    }
                }
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
            tracing::error!("alert mail delivery abandoned after retries");
        });
    }

    fn emit_webhook_alert(&self, payload: &serde_json::Value) {
        let Some(url) = self.webhook_url.as_ref() else {
            return;
        };
        let url = url.clone();
        let http = self.http.clone();
        let payload = payload.clone();
        crate::utils::spawn(async move {
            for delay_secs in [1u64, 5u64, 15u64] {
                match http.post(&url).json(&payload).send().await {
                    Ok(resp) if resp.status().is_success() => return,
                    Ok(resp) => {
                        tracing::warn!(status = %resp.status(), %url, "alert webhook non-success");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, %url, "alert webhook post failed");
                    }
                }
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
            tracing::error!(%url, "alert webhook delivery abandoned after retries");
        });
    }

    pub async fn list_active(&self) -> anyhow::Result<Vec<BanRecord>> {
        let now_ts = Utc::now().timestamp();
        let records = Bans::find()
            .filter(security_ban::Column::BannedUntil.gt(now_ts))
            .filter(security_ban::Column::ReleasedAt.is_null())
            .all(&self.db)
            .await?;
        Ok(records)
    }

    pub async fn release(&self, ip: &str, released_by: &str) -> anyhow::Result<bool> {
        let now_ts = Utc::now().timestamp();
        let existing = Bans::find_by_id(ip.to_string()).one(&self.db).await?;
        let Some(existing) = existing else {
            return Ok(false);
        };
        if existing.released_at.is_some() || existing.banned_until <= now_ts {
            return Ok(false);
        }
        let mut active: security_ban::ActiveModel = existing.into();
        active.released_at = Set(Some(now_ts));
        active.released_by = Set(Some(released_by.to_string()));
        active.update(&self.db).await?;
        if let Ok(parsed) = ip.parse::<IpAddr>() {
            self.banned.rcu(|current| {
                let mut map = HashMap::clone(current);
                map.remove(&parsed);
                map
            });
        }
        tracing::info!(ip, released_by, "ban released");
        Ok(true)
    }
}

pub fn ip_from_source(source: &str) -> Option<IpAddr> {
    let trimmed = source.trim();
    let host_port = trimmed.rsplit_once(':').map(|(h, _)| h).unwrap_or(trimmed);
    let host = match host_port.rsplit_once(' ') {
        Some((_, host)) => host,
        None => host_port,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustpbx_models::migration::Migrator as ModelsMigrator;
    use sea_orm_migration::MigratorTrait;

    #[test]
    fn source_address_parsing_handles_transport_prefixes() {
        assert_eq!(ip_from_source("127.0.0.1:5060"), Some(ip("127.0.0.1")));
        assert_eq!(ip_from_source("UDP 127.0.0.1:18516"), Some(ip("127.0.0.1")));
        assert_eq!(ip_from_source("TCP 10.1.2.3:5060"), Some(ip("10.1.2.3")));
        assert_eq!(ip_from_source("[::1]:5060"), Some(ip("::1")));
        assert_eq!(ip_from_source("TLS [2001:db8::1]:5061"), Some(ip("2001:db8::1")));
        assert_eq!(ip_from_source("unknown"), None);
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    async fn migrated_db() -> sea_orm::DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        ModelsMigrator::up(&db, None).await.unwrap();
        db
    }

    fn cfg(max_failures: u32, durations: Vec<u64>, protected_cidrs: Vec<String>) -> BanConfig {
        BanConfig {
            enabled: true,
            max_failures,
            window_secs: 600,
            ban_durations_secs: durations,
            protected_cidrs,
        }
    }

    struct FakeClock {
        base: Instant,
        secs: Arc<AtomicU64>,
    }

    impl FakeClock {
        fn new() -> (Self, NowFn) {
            let base = Instant::now();
            let secs = Arc::new(AtomicU64::new(0));
            let secs_move = secs.clone();
            let now: NowFn = Arc::new(move || base + Duration::from_secs(secs_move.load(Ordering::Relaxed)));
            (Self { base, secs }, now)
        }

        fn advance(&self, secs: u64) {
            self.secs.fetch_add(secs, Ordering::Relaxed);
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn bans_after_threshold_and_expires() {
        let db = migrated_db().await;
        let (clock, now) = FakeClock::new();
        let store = BanStore::with_clock(db, cfg(3, vec![60], vec![]), None, now);
        let target = ip("9.9.9.9");

        store.record_auth_failure(target, "u1", "REGISTER").await;
        store.record_auth_failure(target, "u1", "REGISTER").await;
        assert!(!store.is_banned(&target));

        store.record_auth_failure(target, "u1", "REGISTER").await;
        assert!(store.is_banned(&target));

        clock.advance(61);
        assert!(!store.is_banned(&target));
    }

    #[tokio::test]
    async fn escalation_persists_across_store_instances() {
        let dir = std::env::temp_dir().join(format!("rustpbx_ban_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/bans.db?mode=rwc", dir.display());

        let db1 = sea_orm::Database::connect(&url).await.unwrap();
        rustpbx_models::migration::Migrator::up(&db1, None).await.unwrap();
        let (clock1, now1) = FakeClock::new();
        let store1 = BanStore::with_clock(db1, cfg(2, vec![60, 120], vec![]), None, now1);
        let target = ip("8.8.8.8");
        store1.record_auth_failure(target, "u", "REGISTER").await;
        store1.record_auth_failure(target, "u", "REGISTER").await;
        assert!(store1.is_banned(&target));
        store1.db.close().await.unwrap();

        let db2 = sea_orm::Database::connect(&url).await.unwrap();
        let (clock2, now2) = FakeClock::new();
        let store2 = BanStore::with_clock(db2.clone(), cfg(2, vec![60, 120], vec![]), None, now2);
        store2.reload().await.unwrap();
        assert!(store2.is_banned(&target), "ban must survive restart");

        store2.record_auth_failure(target, "u", "INVITE").await;
        store2.record_auth_failure(target, "u", "INVITE").await;
        assert!(store2.is_banned(&target));

        let rows = store2.list_active().await.unwrap();
        let row = rows.iter().find(|r| r.ip == "8.8.8.8").unwrap();
        assert_eq!(row.offense_count, 2, "second offense must escalate");

        assert!(store2.release("8.8.8.8", "test").await.unwrap());
        assert!(!store2.is_banned(&target));
        db2.close().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn protected_cidrs_are_never_banned() {
        let db = migrated_db().await;
        let (_clock, now) = FakeClock::new();
        let store = BanStore::with_clock(
            db,
            cfg(2, vec![60], vec!["10.0.0.0/8".to_string()]),
            None,
            now,
        );
        let internal = ip("10.1.2.3");
        for _ in 0..5 {
            store.record_auth_failure(internal, "u", "REGISTER").await;
        }
        assert!(!store.is_banned(&internal));

        let external = ip("203.0.113.7");
        store.record_auth_failure(external, "u", "REGISTER").await;
        store.record_auth_failure(external, "u", "REGISTER").await;
        assert!(store.is_banned(&external));
    }

    #[derive(Default)]
    struct CapturingMail {
        messages: parking_lot::Mutex<Vec<(Vec<String>, String)>>,
        bodies: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::mail::MailTransport for CapturingMail {
        async fn send(&self, mail: &crate::mail::OutboundMail) -> anyhow::Result<()> {
            self.messages
                .lock()
                .push((mail.to.clone(), mail.subject.clone()));
            self.bodies.lock().push(mail.body.clone());
            Ok(())
        }
    }

    struct TemplateBrand;

    impl crate::branding::BrandingProvider for TemplateBrand {
        fn brand(&self) -> crate::branding::BrandContext {
            crate::branding::BrandContext::default()
        }

        fn email_template(&self) -> Option<crate::mail::EmailTemplate> {
            Some(crate::mail::EmailTemplate {
                subject: "alert {{alert_type}} at {{ip}}".to_string(),
                body: "type={{alert_type}}\nip={{ip}}\ndata={{payload}}".to_string(),
            })
        }
    }

    async fn wait_for_mail(capture: &CapturingMail) {
        for _ in 0..100 {
            if !capture.messages.lock().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn ban_emits_mail_when_configured() {
        let db = migrated_db().await;
        let (_clock, now) = FakeClock::new();
        let capture = Arc::new(CapturingMail::default());
        let store = BanStore::with_clock(db, cfg(2, vec![60], vec![]), None, now).with_mail(
            capture.clone() as Arc<dyn crate::mail::MailTransport>,
            vec!["ops@example.com".to_string()],
        );
        let target = ip("7.7.7.7");
        store.record_auth_failure(target, "u", "REGISTER").await;
        store.record_auth_failure(target, "u", "REGISTER").await;

        for _ in 0..100 {
            if !capture.messages.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let messages = capture.messages.lock();
        assert_eq!(messages.len(), 1, "ban must produce exactly one mail");
        assert_eq!(messages[0].0, vec!["ops@example.com".to_string()]);
        assert!(messages[0].1.starts_with("rustpbx auth_ban"));
    }

    struct PartialTemplateBrand;

    impl crate::branding::BrandingProvider for PartialTemplateBrand {
        fn brand(&self) -> crate::branding::BrandContext {
            crate::branding::BrandContext::default()
        }

        fn email_template(&self) -> Option<crate::mail::EmailTemplate> {
            Some(crate::mail::EmailTemplate {
                subject: String::new(),
                body: "custom {{alert_type}} {{ip}}".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn ban_mail_uses_branding_template_when_configured() {
        let db = migrated_db().await;
        let (_clock, now) = FakeClock::new();
        let capture = Arc::new(CapturingMail::default());
        let store = BanStore::with_clock(db, cfg(2, vec![60], vec![]), None, now).with_mail(
            capture.clone() as Arc<dyn crate::mail::MailTransport>,
            vec!["ops@example.com".to_string()],
        );
        let store = Arc::new(store).with_branding(Some(Arc::new(TemplateBrand)));

        let target = ip("7.7.7.7");
        store.record_auth_failure(target, "u", "REGISTER").await;
        store.record_auth_failure(target, "u", "REGISTER").await;
        wait_for_mail(&capture).await;

        let messages = capture.messages.lock();
        let bodies = capture.bodies.lock();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].1, "alert auth_ban at 7.7.7.7");
        assert!(bodies[0].starts_with("type=auth_ban\nip=7.7.7.7\n"));
        assert!(bodies[0].contains("\"type\": \"auth_ban\""));
    }

    #[tokio::test]
    async fn ban_mail_keeps_default_when_no_branding_template() {
        let db = migrated_db().await;
        let (_clock, now) = FakeClock::new();
        let capture = Arc::new(CapturingMail::default());
        let store = BanStore::with_clock(db, cfg(2, vec![60], vec![]), None, now).with_mail(
            capture.clone() as Arc<dyn crate::mail::MailTransport>,
            vec!["ops@example.com".to_string()],
        );

        let target = ip("7.7.7.7");
        store.record_auth_failure(target, "u", "REGISTER").await;
        store.record_auth_failure(target, "u", "REGISTER").await;
        wait_for_mail(&capture).await;

        let messages = capture.messages.lock();
        let bodies = capture.bodies.lock();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].1, "rustpbx auth_ban: 7.7.7.7");
        assert!(bodies[0].starts_with("{\n"));
        assert!(bodies[0].contains("\"type\": \"auth_ban\""));
    }

    #[tokio::test]
    async fn ban_mail_template_falls_back_for_empty_rendered_field() {
        let db = migrated_db().await;
        let (_clock, now) = FakeClock::new();
        let capture = Arc::new(CapturingMail::default());
        let store = BanStore::with_clock(db, cfg(2, vec![60], vec![]), None, now).with_mail(
            capture.clone() as Arc<dyn crate::mail::MailTransport>,
            vec!["ops@example.com".to_string()],
        );
        let store = Arc::new(store).with_branding(Some(Arc::new(PartialTemplateBrand)));

        let target = ip("7.7.7.7");
        store.record_auth_failure(target, "u", "REGISTER").await;
        store.record_auth_failure(target, "u", "REGISTER").await;
        wait_for_mail(&capture).await;

        let messages = capture.messages.lock();
        let bodies = capture.bodies.lock();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].1, "rustpbx auth_ban: 7.7.7.7");
        assert_eq!(bodies[0], "custom auth_ban 7.7.7.7");
    }
}
