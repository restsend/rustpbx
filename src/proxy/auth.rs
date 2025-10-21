use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::TransactionCookie;
use crate::call::user::SipUser;
use crate::config::ProxyConfig;
use anyhow::Result;
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

#[async_trait]
pub trait AuthBackend: Send + Sync {
    async fn authenticate(&self, original: &rsip::Request) -> Result<Option<SipUser>>;
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

    pub async fn authenticate_request(&self, tx: &Transaction) -> Result<Option<SipUser>> {
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
        let user = SipUser::try_from(tx)?;
        // Check if user exists and is enabled
        match self
            .server
            .user_backend
            .get_user(&user.username, user.realm.as_deref())
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

        if cookie.is_from_trunk() {
            cookie.set_user(tx_user.clone());
            return Ok(ProxyAction::Continue);
        }

        if let Some(backend) = self.server.auth_backend.as_ref() {
            match backend.authenticate(&tx.original).await {
                Ok(Some(mut user)) => {
                    user.merge_with(&tx_user);
                    cookie.set_user(user);
                    return Ok(ProxyAction::Continue);
                }
                Err(e) => {
                    info!(error=%e, key = %tx.key, "auth_backend authenticate failed");
                }
                _ => {}
            }
        }

        match self.authenticate_request(tx).await {
            Ok(authenticated) => {
                if let Some(user) = authenticated {
                    cookie.set_user(user);
                    return Ok(ProxyAction::Continue);
                }

                if tx.original.method == rsip::Method::Invite {
                    if let Ok(to_header) = tx.original.to_header() {
                        if let Ok(to_uri) = to_header.uri() {
                            if let Some(callee_user_raw) = to_uri.user() {
                                let callee_user = callee_user_raw.to_string();
                                let callee_realm = to_uri.host().to_string();
                                let realm_ref = if callee_realm.is_empty() {
                                    None
                                } else {
                                    Some(callee_realm.as_str())
                                };
                                match self
                                    .server
                                    .user_backend
                                    .get_user(&callee_user, realm_ref)
                                    .await
                                {
                                    Ok(Some(callee_profile))
                                        if callee_profile.allow_guest_calls =>
                                    {
                                        info!(
                                            caller = %tx_user.username,
                                            extension = %callee_user,
                                            "Allowing guest call without authentication"
                                        );
                                        cookie.set_user(tx_user.clone());
                                        return Ok(ProxyAction::Continue);
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        info!(
                                            extension = %callee_user,
                                            error = %e,
                                            "Failed to evaluate guest call permission"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                let from_uri = tx.original.from_header()?.uri()?;
                let request_host = tx.original.uri().host().to_string();
                let realm = self.server.proxy_config.select_realm(request_host.as_str());
                // Check which type of authentication was attempted or send both challenges
                let has_proxy_auth_header =
                    rsip::header_opt!(tx.original.headers.iter(), Header::ProxyAuthorization)
                        .is_some();
                if has_proxy_auth_header {
                    // Send proxy challenge if proxy auth was attempted
                    let proxy_auth = self.create_proxy_auth_challenge(&realm)?;
                    info!(
                        from = from_uri.to_string(),
                        realm = realm,
                        ?proxy_auth,
                        "Proxy authentication failed, sending proxy challenge"
                    );
                    let headers = vec![Header::ProxyAuthenticate(proxy_auth)];
                    tx.reply_with(rsip::StatusCode::ProxyAuthenticationRequired, headers, None)
                        .await
                        .ok();
                } else {
                    // Send WWW challenge if WWW auth was attempted
                    let www_auth = self.create_www_auth_challenge(&realm)?;
                    info!(
                        from = from_uri.to_string(),
                        realm = realm,
                        ?www_auth,
                        "WWW authentication failed, sending WWW challenge"
                    );
                    let headers = vec![Header::WwwAuthenticate(www_auth)];
                    tx.reply_with(rsip::StatusCode::Unauthorized, headers, None)
                        .await
                        .ok();
                }
                Ok(ProxyAction::Abort)
            }
            Err(e) => {
                info!(error=%e, key = %tx.key, "Authentication error");
                Err(e)
            }
        }
    }
}
