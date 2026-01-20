use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::cookie::SpamResult;
use crate::call::user::SipUser;
use crate::call::{CalleeDisplayName, TransactionCookie, TrunkContext};
use crate::config::ProxyConfig;
use anyhow::{Error, Result};
use async_trait::async_trait;
use rsip::Header;
use rsip::Uri;
use rsip::headers::UntypedHeader;
use rsip::headers::auth::Algorithm;
use rsip::headers::{ProxyAuthenticate, WwwAuthenticate};
use rsip::prelude::HeadersExt;
use rsip::prelude::ToTypedHeader;
use rsip::services::DigestGenerator;
use rsip::typed::Authorization;
use rsipstack::transaction::transaction::Transaction;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Debug)]
pub enum AuthError {
    NotFound,
    Disabled,
    InvalidCredentials,
    SpamDetected,
    PaymentRequired,
    Other(Error),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::NotFound => write!(f, "User not found"),
            AuthError::InvalidCredentials => write!(f, "Invalid credentials"),
            AuthError::SpamDetected => write!(f, "Spam detected"),
            AuthError::PaymentRequired => write!(f, "Payment required"),
            AuthError::Disabled => write!(f, "User is disabled"),
            AuthError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AuthError::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<Error> for AuthError {
    fn from(e: Error) -> Self {
        AuthError::Other(e)
    }
}

#[async_trait]
pub trait AuthBackend: Send + Sync {
    async fn authenticate(
        &self,
        original: &rsip::Request,
        cookie: &TransactionCookie,
    ) -> Result<Option<SipUser>, AuthError>;
}

#[derive(Clone)]
pub struct AuthModule {
    server: SipServerRef,
}

impl AuthModule {
    pub fn create(server: SipServerRef, _config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = AuthModule::new(server);
        Ok(Box::new(module))
    }

    pub fn new(server: SipServerRef) -> Self {
        Self { server }
    }

    pub async fn authenticate_request(
        &self,
        tx: &Transaction,
    ) -> Result<Option<SipUser>, AuthError> {
        let mut auth_inner: Option<Authorization> = None;
        for header in tx.original.headers.iter() {
            match header {
                Header::Authorization(h) => {
                    auth_inner = h.typed().ok();
                    break;
                }
                Header::ProxyAuthorization(h) => {
                    auth_inner = h.typed().ok().map(|a| a.0);
                    break;
                }
                _ => {}
            }
        }
        let auth_inner = match auth_inner {
            Some(a) => a,
            None => {
                return Ok(None);
            }
        };
        let user = SipUser::try_from(tx).map_err(|e| AuthError::Other(e))?;
        // Check if user exists and is enabled
        match self
            .server
            .user_backend
            .get_user(&user.username, user.realm.as_deref(), Some(&tx.original))
            .await?
        {
            Some(mut stored_user) => {
                if !stored_user.enabled {
                    info!(username = user.username, realm = ?user.realm, "User is disabled");
                    return Ok(None);
                }
                if let Some(realm) = user.realm.as_ref() {
                    if !self.server.is_same_realm(realm).await {
                        info!(username = user.username, realm = ?user.realm, "User is not in the same realm");
                        return Ok(None);
                    }
                }
                stored_user.merge_with(&user);
                match self.verify_credentials(
                    &stored_user,
                    &tx.original.uri,
                    &tx.original.method,
                    &auth_inner,
                ) {
                    true => Ok(Some(stored_user)),
                    false => Ok(None),
                }
            }
            None => {
                info!(username = user.username, realm = ?user.realm, "authenticate_request missing");
                Ok(None)
            }
        }
    }

    fn verify_credentials(
        &self,
        user: &SipUser,
        uri: &Uri,
        method: &rsip::Method,
        auth: &Authorization,
    ) -> bool {
        // Use the same approach as common.rs
        let empty_string = "".to_string();
        let password = user.password.as_ref().unwrap_or(&empty_string);

        // Create a digest generator to compute the expected response
        let expected_response = DigestGenerator {
            username: &user.username,
            password,
            algorithm: auth.algorithm.unwrap_or(Algorithm::Md5),
            nonce: &auth.nonce,
            method,
            qop: auth.qop.as_ref(),
            uri,
            realm: &auth.realm,
        }
        .compute();

        expected_response == auth.response
    }

    pub fn create_proxy_auth_challenge(&self, realm: &str) -> Result<ProxyAuthenticate> {
        let nonce = rsipstack::transaction::random_text(16);
        let proxy_auth = ProxyAuthenticate::new(format!(
            r#"Digest realm="{}", nonce="{}", algorithm=MD5"#,
            realm, nonce
        ));
        Ok(proxy_auth)
    }

    pub fn create_www_auth_challenge(&self, realm: &str) -> Result<WwwAuthenticate> {
        let nonce = rsipstack::transaction::random_text(16);
        let www_auth = WwwAuthenticate::new(format!(
            r#"Digest realm="{}", nonce="{}", algorithm=MD5"#,
            realm, nonce
        ));
        Ok(www_auth)
    }
}

#[async_trait]
impl ProxyModule for AuthModule {
    fn name(&self) -> &str {
        "auth"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Register,
            rsip::Method::Bye,
            rsip::Method::Options,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        debug!("Auth module started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        debug!("Auth module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        // Only authenticate INVITE and REGISTER requests
        if tx.original.method != rsip::Method::Invite
            && tx.original.method != rsip::Method::Register
        {
            return Ok(ProxyAction::Continue);
        }

        let tx_user = SipUser::try_from(&*tx)?;
        let source = tx_user
            .destination
            .as_ref()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        for backend in self.server.auth_backend.iter() {
            match backend.authenticate(&tx.original, &cookie).await {
                Ok(Some(mut user)) => {
                    user.merge_with(&tx_user);
                    cookie.set_user(user);
                    return Ok(ProxyAction::Continue);
                }
                Err(e) => {
                    if matches!(e, AuthError::SpamDetected) {
                        cookie.mark_as_spam(SpamResult::Spam);
                    }
                    info!(error=%e, key = %tx.key, %source, "auth_backend authenticate failed");
                }
                _ => {}
            }
        }

        if cookie.is_spam() {
            return Ok(ProxyAction::Abort);
        }

        if cookie.get_extension::<TrunkContext>().is_some() {
            cookie.set_user(tx_user.clone());
            return Ok(ProxyAction::Continue);
        }

        match self.authenticate_request(tx).await {
            Ok(authenticated) => {
                if let Some(user) = authenticated {
                    cookie.set_user(user);
                    return Ok(ProxyAction::Continue);
                }

                let to_header = tx.original.to_header()?.uri()?;
                let callee_user = to_header.user().unwrap_or_else(|| "");
                let callee_realm = to_header.host().to_string();

                if tx.original.method == rsip::Method::Invite {
                    match self
                        .server
                        .user_backend
                        .get_user(&callee_user, Some(&callee_realm), Some(&tx.original))
                        .await
                    {
                        Ok(Some(callee_profile)) if callee_profile.allow_guest_calls => {
                            info!(
                                caller = %tx_user.username,
                                extension = %callee_user,
                                %source,
                                "Allowing guest call without authentication"
                            );
                            cookie.set_user(tx_user.clone());
                            if let Some(display_name) = callee_profile.display_name {
                                cookie.insert_extension(CalleeDisplayName(display_name));
                            }
                            return Ok(ProxyAction::Continue);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            info!(
                                extension = %callee_user,
                                error = %e,
                                %source,
                                "Failed to evaluate guest call permission"
                            );
                        }
                    }
                }

                let from_uri = tx.original.from_header()?.uri()?;
                let request_host = tx.original.uri().host().to_string();
                let realm = self.server.proxy_config.select_realm(request_host.as_str());

                if self.server.proxy_config.ensure_user.unwrap_or_default() {
                    match self
                        .server
                        .user_backend
                        .get_user(
                            &from_uri.user().unwrap_or_else(|| ""),
                            Some(&realm),
                            Some(&tx.original),
                        )
                        .await
                    {
                        Ok(Some(_)) => {}
                        _ => {
                            info!(
                                from = %from_uri,
                                %source,
                                "User not found, don't send authentication challenge"
                            );
                            cookie.mark_as_spam(SpamResult::Spam);
                            return Ok(ProxyAction::Abort);
                        }
                    };
                }

                let (status_code, headers) = if tx.original.method == rsip::Method::Register {
                    let www_auth = self.create_www_auth_challenge(&realm)?;
                    (
                        rsip::StatusCode::Unauthorized,
                        vec![Header::WwwAuthenticate(www_auth)],
                    )
                } else {
                    let www_auth = self.create_www_auth_challenge(&realm)?;
                    let proxy_auth = self.create_proxy_auth_challenge(&realm)?;
                    (
                        rsip::StatusCode::ProxyAuthenticationRequired,
                        vec![
                            Header::WwwAuthenticate(www_auth),
                            Header::ProxyAuthenticate(proxy_auth),
                        ],
                    )
                };

                info!(
                    from = from_uri.to_string(),
                    realm = realm,
                    status = %status_code,
                    %source,
                    "Authentication failed, sending challenge"
                );
                tx.reply_with(status_code, headers, None).await.ok();
                Ok(ProxyAction::Abort)
            }
            Err(e) => {
                info!(error=%e, key = %tx.key, %source, "Authentication error");
                if matches!(e, AuthError::SpamDetected) {
                    cookie.mark_as_spam(SpamResult::Spam);
                }
                Err(anyhow::anyhow!("Authentication error: {}", e))
            }
        }
    }
}
