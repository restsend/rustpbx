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
use hmac::{Hmac, Mac};
use sea_orm::sea_query::Condition;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    TransactionTrait,
};
use sha2::Sha256;
use std::time::Duration;
use tracing::warn;

pub(super) const SESSION_COOKIE_NAME: &str = "rustpbx_session";
const SESSION_TTL_HOURS: u64 = 12;
const RESET_TOKEN_VALID_MINUTES: u64 = 30;

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
        let _is_secure = self.config.secure_cookie || request_secure;
        // let secure_attr = if is_secure { "; Secure" } else { "" };
        let secure_attr = "";
        let cookie = format!(
            "{}=; Path={}; HttpOnly; Max-Age=0{}",
            SESSION_COOKIE_NAME,
            self.cookie_path(),
            secure_attr
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
        let _is_secure = self.config.secure_cookie || request_secure;
        let secure_attr = "";
        let cookie = format!(
            "{}={}; Path={}; HttpOnly; Max-Age={}{}",
            SESSION_COOKIE_NAME,
            value,
            self.cookie_path(),
            SESSION_TTL_HOURS * 3600,
            secure_attr
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
        let mut model = <UserActiveModel as Default>::default();
        model.email = Set(email.to_string());
        model.username = Set(username.to_string());
        model.password_hash = Set(hashed);
        model.created_at = Set(now);
        model.updated_at = Set(now);
        model.is_active = Set(true);
        let is_first_user = existing_users == 0;
        model.is_staff = Set(is_first_user);
        model.is_superuser = Set(is_first_user);
        model.reset_token = Set(None);
        model.reset_token_expires = Set(None);

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
            Arc::new(crate::callrecord::DefaultCallRecordFormatter::default()),
            db,
            ConsoleConfig {
                session_secret: "secret".into(),
                base_path: "/console".into(),
                allow_registration,
                secure_cookie: false,
                alpine_js: None,
                tailwind_js: None,
                chart_js: None,
            },
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
}
