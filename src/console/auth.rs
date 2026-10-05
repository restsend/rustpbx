use crate::console::ConsoleState;
use crate::models::user::{
    ActiveModel as UserActiveModel, Column as UserColumn, Entity as UserEntity, Model as UserModel,
};
use anyhow::{Context, Result, bail};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use axum::http::HeaderValue;
use base64::engine::{Engine, general_purpose::STANDARD_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sea_orm::sea_query::Condition;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    TransactionTrait,
};
use sha2::Sha256;
use std::time::{Duration, Instant};
use tracing::warn;

pub(super) const SESSION_COOKIE_NAME: &str = "rustpbx_session";
pub(super) const MFA_SESSION_COOKIE_NAME: &str = "rustpbx_mfa";
const SESSION_TTL_HOURS: u64 = 12;
const RESET_TOKEN_VALID_MINUTES: u64 = 30;
const MFA_SESSION_TTL_SECS: u64 = 300; // 5 minutes for MFA verification

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegistrationPolicy {
    pub allowed: bool,
    pub first_user: bool,
}

impl ConsoleState {
    fn sign(&self, payload: &str) -> Option<String> {
        let mut mac = HmacSha256::new_from_slice(self.session_key.as_slice()).ok()?;
        mac.update(payload.as_bytes());
        let signature = mac.finalize().into_bytes();
        Some(STANDARD_NO_PAD.encode(signature))
    }

    pub(super) fn session_user_id(&self, cookie_value: Option<&str>) -> Option<i64> {
        let value = cookie_value?;
        let mut segments = value.split(':');
        let user_id: i64 = segments.next()?.parse().ok()?;
        let expires: i64 = segments.next()?.parse().ok()?;
        let signature = segments.next()?;
        if segments.next().is_some() {
            return None;
        }
        if expires <= Utc::now().timestamp() {
            return None;
        }
        let payload = format!("{}:{}", user_id, expires);
        let expected = self.sign(&payload)?;
        if expected != signature {
            return None;
        }
        Some(user_id)
    }

    pub fn clear_session_cookie(&self, request_secure: bool) -> Option<HeaderValue> {
        let suffix = self.cookie_suffix(request_secure);
        let cookie = format!(
            "{}=; Path={}; HttpOnly; Max-Age=0{}",
            SESSION_COOKIE_NAME,
            self.cookie_path(),
            suffix
        );
        match HeaderValue::from_str(&cookie) {
            Ok(header) => Some(header),
            Err(err) => {
                warn!("failed to build clear-session cookie header: {}", err);
                None
            }
        }
    }

    pub fn generate_session_token(&self, user_id: i64) -> Option<String> {
        let expires_at = Utc::now() + Duration::from_secs(SESSION_TTL_HOURS * 3600);
        let payload = format!("{}:{}", user_id, expires_at.timestamp());
        let signature = match self.sign(&payload) {
            Some(sig) => sig,
            None => {
                warn!("failed to sign session payload");
                return None;
            }
        };
        Some(format!("{}:{}", payload, signature))
    }

    pub fn session_cookie_header(&self, user_id: i64, request_secure: bool) -> Option<HeaderValue> {
        let value = self.generate_session_token(user_id)?;
        let suffix = self.cookie_suffix(request_secure);
        let cookie = format!(
            "{}={}; Path={}; HttpOnly; Max-Age={}{}",
            SESSION_COOKIE_NAME,
            value,
            self.cookie_path(),
            SESSION_TTL_HOURS * 3600,
            suffix
        );
        match HeaderValue::from_str(&cookie) {
            Ok(header) => Some(header),
            Err(err) => {
                warn!("failed to build session cookie header: {}", err);
                None
            }
        }
    }

    fn cookie_path(&self) -> &str {
        // Always scope the session cookie to the site root so AMI requests outside the
        // console base path still receive it.
        "/"
    }

    /// Build the cookie attribute suffix: always `SameSite=Lax`, plus `Secure`
    /// when the request was over TLS (or `secure_cookie` is forced in config).
    fn cookie_suffix(&self, request_secure: bool) -> String {
        let is_secure = self.config.secure_cookie || request_secure;
        format!("; SameSite=Lax{}", if is_secure { "; Secure" } else { "" })
    }

    /// Generate a temporary MFA verification session token
    pub fn generate_mfa_session_token(&self, user_id: i64) -> Option<String> {
        let expires_at = Utc::now() + Duration::from_secs(MFA_SESSION_TTL_SECS);
        let payload = format!("{}:{}:mfa", user_id, expires_at.timestamp());
        let signature = match self.sign(&payload) {
            Some(sig) => sig,
            None => {
                warn!("failed to sign MFA session payload");
                return None;
            }
        };
        Some(format!("{}:{}", payload, signature))
    }

    /// Verify MFA session token and return user_id if valid
    pub fn verify_mfa_session_token(&self, cookie_value: Option<&str>) -> Option<i64> {
        let value = cookie_value?;
        let mut segments = value.split(':');
        let user_id: i64 = segments.next()?.parse().ok()?;
        let expires: i64 = segments.next()?.parse().ok()?;
        let _mfa_flag = segments.next()?;
        let signature = segments.next()?;
        if segments.next().is_some() {
            return None;
        }
        if expires <= Utc::now().timestamp() {
            return None;
        }
        let payload = format!("{}:{}:mfa", user_id, expires);
        let expected = self.sign(&payload)?;
        if expected != signature {
            return None;
        }
        Some(user_id)
    }

    /// Create MFA session cookie for temporary verification
    pub fn mfa_session_cookie_header(
        &self,
        user_id: i64,
        request_secure: bool,
    ) -> Option<HeaderValue> {
        let value = self.generate_mfa_session_token(user_id)?;
        let suffix = self.cookie_suffix(request_secure);
        let cookie = format!(
            "{}={}; Path={}; HttpOnly; Max-Age={}{}",
            MFA_SESSION_COOKIE_NAME,
            value,
            self.cookie_path(),
            MFA_SESSION_TTL_SECS,
            suffix
        );
        match HeaderValue::from_str(&cookie) {
            Ok(header) => Some(header),
            Err(err) => {
                warn!("failed to build MFA session cookie header: {}", err);
                None
            }
        }
    }

    /// Clear MFA session cookie
    pub fn clear_mfa_session_cookie(&self, request_secure: bool) -> Option<HeaderValue> {
        let suffix = self.cookie_suffix(request_secure);
        let cookie = format!(
            "{}={}; Path={}; HttpOnly; Max-Age=0{}",
            MFA_SESSION_COOKIE_NAME,
            "",
            self.cookie_path(),
            suffix
        );
        match HeaderValue::from_str(&cookie) {
            Ok(header) => Some(header),
            Err(err) => {
                warn!("failed to build clear-MFA-session cookie header: {}", err);
                None
            }
        }
    }

    pub async fn registration_policy(&self) -> Result<RegistrationPolicy> {
        let total_users = UserEntity::find()
            .count(&self.db)
            .await
            .context("failed to count existing console users")?;

        if total_users == 0 {
            Ok(RegistrationPolicy {
                allowed: true,
                first_user: true,
            })
        } else {
            Ok(RegistrationPolicy {
                allowed: self.config.allow_registration,
                first_user: false,
            })
        }
    }

    pub async fn authenticate(
        &self,
        identifier: &str,
        password: &str,
    ) -> Result<Option<UserModel>> {
        // Commercial Addon Authentication (Enterprise Auth strategy)
        let auth_app_state = if let Ok(guard) = self.app_state.read() {
            guard.as_ref().and_then(|weak| weak.upgrade())
        } else {
            None
        };

        if let Some(app_state) = auth_app_state
            && let Ok(Some(user)) = app_state
                .addon_registry
                .authenticate_all(app_state.clone(), identifier, password)
                .await
        {
            tracing::info!(
                "User {} authenticated via Enterprise Auth addon",
                user.username
            );
            return Ok(Some(user));
        }

        let trimmed = identifier.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let email_candidate = trimmed.to_lowercase();
        let condition = Condition::any()
            .add(UserColumn::Email.eq(email_candidate.clone()))
            .add(UserColumn::Username.eq(trimmed));

        let user = UserEntity::find()
            .filter(condition)
            .one(&self.db)
            .await
            .context("failed to query user for authentication")?;

        if let Some(user) = user {
            if !user.is_active {
                return Ok(None);
            }
            let parsed = PasswordHash::new(&user.password_hash)
                .map_err(|e| anyhow::anyhow!("invalid stored password hash: {}", e))?;
            if Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
            {
                return Ok(Some(user));
            }
        }

        Ok(None)
    }

    pub async fn email_exists(&self, email: &str) -> Result<bool> {
        let user = UserEntity::find()
            .filter(UserColumn::Email.eq(email))
            .one(&self.db)
            .await
            .context("failed to check email uniqueness")?;
        Ok(user.is_some())
    }

    pub async fn find_user_by_email(&self, email: &str) -> Result<Option<UserModel>> {
        let user = UserEntity::find()
            .filter(UserColumn::Email.eq(email))
            .one(&self.db)
            .await
            .context("failed to lookup user by email")?;
        Ok(user)
    }

    pub async fn username_exists(&self, username: &str) -> Result<bool> {
        let user = UserEntity::find()
            .filter(UserColumn::Username.eq(username))
            .one(&self.db)
            .await
            .context("failed to check username uniqueness")?;
        Ok(user.is_some())
    }

    pub async fn create_user(
        &self,
        email: &str,
        username: &str,
        password: &str,
    ) -> Result<UserModel> {
        let tx = self
            .db
            .begin()
            .await
            .context("failed to start user creation transaction")?;

        let existing_users = UserEntity::find()
            .count(&tx)
            .await
            .context("failed to count existing console users")?;

        if existing_users > 0 && !self.registration_allowed_by_config() {
            tx.rollback().await.ok();
            bail!("self-service registration is disabled");
        }

        let salt = SaltString::generate(&mut OsRng);
        let hashed = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("failed to hash password: {}", e))?
            .to_string();

        let now = Utc::now();
        let is_first_user = existing_users == 0;
        let model = UserActiveModel {
            email: Set(email.to_string()),
            username: Set(username.to_string()),
            password_hash: Set(hashed),
            created_at: Set(now),
            updated_at: Set(now),
            is_active: Set(true),
            is_staff: Set(is_first_user),
            is_superuser: Set(is_first_user),
            reset_token: Set(None),
            reset_token_expires: Set(None),
            ..Default::default()
        };

        let created = model
            .insert(&tx)
            .await
            .context("failed to insert new user")?;

        tx.commit()
            .await
            .context("failed to commit user creation transaction")?;

        Ok(created)
    }

    pub async fn upsert_reset_token(&self, user: &UserModel) -> Result<(String, DateTime<Utc>)> {
        let token = uuid::Uuid::new_v4().to_string();
        let expires = Utc::now() + Duration::from_secs(RESET_TOKEN_VALID_MINUTES * 60);
        let mut model: UserActiveModel = user.clone().into();
        model.reset_token = Set(Some(token.clone()));
        model.reset_token_expires = Set(Some(expires));
        model.updated_at = Set(Utc::now());
        model
            .update(&self.db)
            .await
            .context("failed to persist reset token")?;
        Ok((token, expires))
    }

    pub async fn find_by_reset_token(&self, token: &str) -> Result<Option<UserModel>> {
        let user = UserEntity::find()
            .filter(UserColumn::ResetToken.eq(token))
            .one(&self.db)
            .await
            .context("failed to lookup reset token")?;
        Ok(user)
    }

    pub async fn update_password(&self, user: &UserModel, new_password: &str) -> Result<UserModel> {
        let salt = SaltString::generate(&mut OsRng);
        let hashed = Argon2::default()
            .hash_password(new_password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("failed to hash password: {}", e))?
            .to_string();
        let now = Utc::now();
        let mut model: UserActiveModel = user.clone().into();
        model.password_hash = Set(hashed);
        model.reset_token = Set(None);
        model.reset_token_expires = Set(None);
        model.updated_at = Set(now);
        model
            .update(&self.db)
            .await
            .context("failed to update password")
    }

    pub async fn mark_login(&self, user: &UserModel, last_login_ip: String) -> Result<()> {
        let mut model: UserActiveModel = user.clone().into();
        let now = Utc::now();
        model.last_login_at = Set(Some(now));
        model.last_login_ip = Set(Some(last_login_ip));
        model.updated_at = Set(now);
        model
            .update(&self.db)
            .await
            .context("failed to update last_login_at")?;
        Ok(())
    }

    pub async fn current_user(&self, cookie_value: Option<&str>) -> Result<Option<UserModel>> {
        if let Some(user_id) = self.session_user_id(cookie_value) {
            let user = UserEntity::find_by_id(user_id)
                .one(&self.db)
                .await
                .context("failed to lookup current user")?;
            Ok(user.filter(|u| u.is_active))
        } else {
            Ok(None)
        }
    }

    /// Enable MFA for a user with the provided secret and verify the first code
    pub async fn enable_mfa(&self, user: &UserModel, secret: &str, code: &str) -> Result<bool> {
        use totp_rs::{Algorithm, Secret, TOTP};

        let secret_bytes = match Secret::Encoded(secret.to_string()).to_bytes() {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };

        let totp = match TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            secret_bytes,
            Some(crate::config::BRAND_NAME.to_string()),
            user.email.clone(),
        ) {
            Ok(t) => t,
            Err(_) => return Ok(false),
        };

        if !totp.check_current(code).unwrap_or(false) {
            return Ok(false);
        }

        // Save the secret and enable MFA
        let mut model: UserActiveModel = user.clone().into();
        model.mfa_enabled = Set(true);
        model.mfa_secret = Set(Some(secret.to_string()));
        model.updated_at = Set(Utc::now());
        model.update(&self.db).await?;

        Ok(true)
    }

    /// Disable MFA for a user
    pub async fn disable_mfa(&self, user: &UserModel) -> Result<UserModel> {
        let mut model: UserActiveModel = user.clone().into();
        model.mfa_enabled = Set(false);
        model.mfa_secret = Set(None);
        model.updated_at = Set(Utc::now());
        model
            .update(&self.db)
            .await
            .context("failed to disable MFA")
    }

    pub fn verify_user_password(user: &UserModel, password: &str) -> bool {
        match PasswordHash::new(&user.password_hash) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }

    pub fn mfa_is_locked(&self, user_id: i64) -> bool {
        let mut attempts = match self.mfa_attempts.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        if let Some(record) = attempts.get_mut(&user_id)
            && let Some(until) = record.locked_until
        {
            if Instant::now() < until {
                return true;
            }
            record.locked_until = None;
            record.failures = 0;
        }
        false
    }

    pub fn mfa_record_failure(&self, user_id: i64) -> bool {
        let mut attempts = match self.mfa_attempts.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let record = attempts.entry(user_id).or_default();
        record.failures += 1;
        if record.failures >= crate::console::MFA_MAX_ATTEMPTS {
            record.locked_until =
                Some(Instant::now() + Duration::from_secs(crate::console::MFA_LOCKOUT_SECS));
            record.failures = 0;
            return true;
        }
        false
    }

    pub fn mfa_clear_failures(&self, user_id: i64) {
        if let Ok(mut attempts) = self.mfa_attempts.lock() {
            attempts.remove(&user_id);
        }
    }

    pub fn mfa_lockout_remaining_secs(&self, user_id: i64) -> Option<u64> {
        let attempts = self.mfa_attempts.lock().ok()?;
        let record = attempts.get(&user_id)?;
        let until = record.locked_until?;
        let now = Instant::now();
        if now < until {
            Some((until - now).as_secs() + 1)
        } else {
            None
        }
    }

    pub(super) fn report_mfa_attempt(
        &self,
        username: &str,
        source: Option<String>,
        outcome: crate::addons::AuthAttemptOutcome,
    ) {
        if let Some(app_state) = self.app_state() {
            app_state
                .addon_registry
                .dispatch_auth_attempt(&crate::addons::AuthAttempt {
                    username: username.to_string(),
                    realm: None,
                    method: "LOGIN_MFA".to_string(),
                    source,
                    outcome,
                });
        }
    }

    /// Verify an MFA code for a user
    pub fn verify_mfa_code(user: &UserModel, code: &str) -> bool {
        use totp_rs::{Algorithm, Secret, TOTP};

        let secret = match &user.mfa_secret {
            Some(s) => s,
            None => return false,
        };

        let secret_bytes = match Secret::Encoded(secret.to_string()).to_bytes() {
            Ok(s) => s,
            Err(_) => return false,
        };

        let totp = match TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            secret_bytes,
            Some(crate::config::BRAND_NAME.to_string()),
            user.email.clone(),
        ) {
            Ok(t) => t,
            Err(_) => return false,
        };

        totp.check_current(code).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ConsoleConfig, models::migration::Migrator};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use std::sync::Arc;

    async fn setup_state(allow_registration: bool) -> Arc<ConsoleState> {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        Migrator::up(&db, None).await.expect("apply migrations");
        ConsoleState::initialize(
            db,
            ConsoleConfig {
                session_secret: "secret".into(),
                base_path: "/console".into(),
                allow_registration,
                secure_cookie: false,
                alpine_js: None,
                tailwind_js: None,
                chart_js: None,
                ..Default::default()
            },
            None,
        )
        .await
        .expect("init console state")
    }

    #[tokio::test]
    async fn registration_policy_allows_initial_user() {
        let state = setup_state(false).await;
        let policy = state.registration_policy().await.expect("policy");
        assert!(policy.allowed);
        assert!(policy.first_user);
    }

    #[tokio::test]
    async fn first_user_becomes_superuser_and_blocks_when_disabled() {
        let state = setup_state(false).await;
        let first = state
            .create_user("owner@rustpbx.com", "owner", "password123")
            .await
            .expect("create first user");
        assert!(first.is_superuser);
        assert!(first.is_staff);

        let policy_after = state.registration_policy().await.expect("policy");
        assert!(!policy_after.allowed);
        assert!(!policy_after.first_user);

        let err = state
            .create_user("second@rustpbx.com", "second", "password123")
            .await
            .expect_err("second user should be blocked");
        assert!(
            err.to_string()
                .contains("self-service registration is disabled")
        );
    }

    #[tokio::test]
    async fn additional_users_allowed_when_enabled() {
        let state = setup_state(true).await;
        let first = state
            .create_user("root@rustpbx.com", "root", "password123")
            .await
            .expect("create first user");
        assert!(first.is_superuser);

        let policy_after = state.registration_policy().await.expect("policy");
        assert!(policy_after.allowed);
        assert!(!policy_after.first_user);

        let second = state
            .create_user("member@rustpbx.com", "member", "password123")
            .await
            .expect("create second user");
        assert!(!second.is_superuser);
        assert!(!second.is_staff);
    }

    fn current_totp_code(secret: &str, email: &str) -> String {
        use totp_rs::{Algorithm, Secret, TOTP};
        let bytes = Secret::Encoded(secret.to_string())
            .to_bytes()
            .expect("valid base32 secret");
        let totp = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            bytes,
            Some(crate::config::BRAND_NAME.to_string()),
            email.to_string(),
        )
        .expect("build totp");
        totp.generate_current().expect("generate code")
    }

    fn enrollment_secret() -> String {
        use totp_rs::Secret;
        Secret::generate_secret().to_encoded().to_string()
    }

    #[tokio::test]
    async fn enable_mfa_with_current_code_persists_and_verifies() {
        let state = setup_state(false).await;
        let user = state
            .create_user("mfa@rustpbx.com", "mfauser", "password123")
            .await
            .expect("create user");
        let secret = enrollment_secret();
        let code = current_totp_code(&secret, &user.email);

        assert!(
            state
                .enable_mfa(&user, &secret, &code)
                .await
                .expect("enable mfa")
        );

        let reloaded = state.find_user_by_email(&user.email).await.expect("reload");
        let reloaded = reloaded.expect("user exists");
        assert!(reloaded.mfa_enabled);
        assert_eq!(reloaded.mfa_secret.as_deref(), Some(secret.as_str()));
        assert!(ConsoleState::verify_mfa_code(&reloaded, &code));
    }

    #[tokio::test]
    async fn enable_mfa_rejects_wrong_code() {
        let state = setup_state(false).await;
        let user = state
            .create_user("wrongcode@rustpbx.com", "wrongcode", "password123")
            .await
            .expect("create user");
        let secret = enrollment_secret();

        assert!(
            !state
                .enable_mfa(&user, &secret, "000000")
                .await
                .expect("enable mfa returns false")
        );

        let reloaded = state.find_user_by_email(&user.email).await.expect("reload");
        let reloaded = reloaded.expect("user exists");
        assert!(!reloaded.mfa_enabled);
        assert!(reloaded.mfa_secret.is_none());
    }

    #[tokio::test]
    async fn disable_mfa_clears_secret_and_verification_fails() {
        let state = setup_state(false).await;
        let user = state
            .create_user("disable@rustpbx.com", "disableuser", "password123")
            .await
            .expect("create user");
        let secret = enrollment_secret();
        let code = current_totp_code(&secret, &user.email);
        state
            .enable_mfa(&user, &secret, &code)
            .await
            .expect("enable mfa");

        let enabled = state.find_user_by_email(&user.email).await.expect("reload");
        let enabled = enabled.expect("user exists");
        state.disable_mfa(&enabled).await.expect("disable mfa");

        let reloaded = state.find_user_by_email(&user.email).await.expect("reload");
        let reloaded = reloaded.expect("user exists");
        assert!(!reloaded.mfa_enabled);
        assert!(reloaded.mfa_secret.is_none());
        assert!(!ConsoleState::verify_mfa_code(&reloaded, &code));
    }

    #[tokio::test]
    async fn mfa_limiter_locks_after_five_failures_and_clears() {
        let db = Database::connect("sqlite::memory:").await.expect("connect");
        Migrator::up(&db, None).await.expect("migrations");
        let state = ConsoleState::initialize(db, ConsoleConfig::default(), None)
            .await
            .expect("init console state");

        assert!(!state.mfa_is_locked(7));
        for _ in 0..(crate::console::MFA_MAX_ATTEMPTS - 1) {
            assert!(!state.mfa_record_failure(7));
            assert!(!state.mfa_is_locked(7));
        }
        assert!(state.mfa_record_failure(7));
        assert!(state.mfa_is_locked(7));

        state.mfa_clear_failures(7);
        assert!(!state.mfa_is_locked(7));
    }
}
