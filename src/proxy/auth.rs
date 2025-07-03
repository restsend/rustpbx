use super::{server::SipServerRef, user::SipUser, ProxyAction, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use rsip::headers::auth::Algorithm;
use rsip::headers::UntypedHeader;
use rsip::headers::{
    ProxyAuthenticate as ProxyAuthenticateHeader, WwwAuthenticate as WwwAuthenticateHeader,
};
use rsip::prelude::HeadersExt;
use rsip::prelude::ToTypedHeader;
use rsip::services::DigestGenerator;
use rsip::typed::Authorization;
use rsip::Header;
use rsip::Uri;
use rsipstack::transaction::transaction::Transaction;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

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

    pub async fn authenticate_request(&self, tx: &mut Transaction) -> Result<bool> {
        // Check for both Authorization and Proxy-Authorization headers
        // Prioritize Authorization for backward compatibility
        let auth_result = AuthModule::check_authorization_headers(&tx.original)?;

        if let Some((user, auth_inner)) = auth_result {
            // Check if user exists and is enabled
            match self
                .server
                .user_backend
                .get_user(&user.username, user.realm.as_deref())
                .await
            {
                Ok(stored_user) => {
                    if !stored_user.enabled {
                        info!(username = user.username, realm = ?user.realm, "User is disabled");
                        return Ok(false);
                    }
                    if let Some(realm) = user.realm.as_ref() {
                        if !self.server.config.is_same_realm(realm) {
                            info!(username = user.username, realm = ?user.realm, "User is not in the same realm");
                            return Ok(false);
                        }
                    }
                    let result = self.verify_credentials(
                        &stored_user,
                        &tx.original.uri,
                        &tx.original.method,
                        &auth_inner,
                    )?;
                    debug!("Credential verification result: {}", result);
                    Ok(result)
                }
                Err(e) => {
                    info!(
                        username = user.username,
                        ?user.realm, "{}", e);
                    Ok(false)
                }
            }
        } else {
            debug!("No authorization headers found");
            Ok(false)
        }
    }

    pub fn check_authorization_headers(
        req: &rsip::Request,
    ) -> Result<Option<(SipUser, Authorization)>> {
        // First try Authorization header (for backward compatibility with existing tests)
        if let Some(auth_header) = rsip::header_opt!(req.headers.iter(), Header::Authorization) {
            let challenge = auth_header.typed()?;
            let user = SipUser {
                username: challenge.username.to_string(),
                realm: Some(challenge.realm.to_string()),
                ..Default::default()
            };
            return Ok(Some((user, challenge)));
        }
        // Then try Proxy-Authorization header
        if let Some(proxy_auth_header) =
            rsip::header_opt!(req.headers.iter(), Header::ProxyAuthorization)
        {
            let challenge = proxy_auth_header.typed()?;
            let user = SipUser {
                username: challenge.0.username.to_string(),
                realm: Some(challenge.0.realm.to_string()),
                ..Default::default()
            };
            return Ok(Some((user, challenge.0)));
        }

        Ok(None)
    }

    fn verify_credentials(
        &self,
        user: &SipUser,
        uri: &Uri,
        method: &rsip::Method,
        auth: &Authorization,
    ) -> Result<bool> {
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

        let result = expected_response == auth.response;
        if result {
            debug!(username = user.username, "Authentication successful");
        } else {
            debug!(
                username = user.username,
                "Authentication failed: response mismatch"
            );
        }

        Ok(result)
    }

    pub fn create_proxy_auth_challenge(&self, realm: &str) -> Result<ProxyAuthenticateHeader> {
        let nonce = rsipstack::transaction::random_text(16);
        let proxy_auth = ProxyAuthenticateHeader::new(format!(
            r#"Digest realm="{}", nonce="{}", algorithm=MD5"#,
            realm, nonce
        ));
        Ok(proxy_auth)
    }

    pub fn create_www_auth_challenge(&self, realm: &str) -> Result<WwwAuthenticateHeader> {
        let nonce = rsipstack::transaction::random_text(16);
        let www_auth = WwwAuthenticateHeader::new(format!(
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
        info!("Auth module started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("Auth module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        // Only authenticate INVITE and REGISTER requests
        if tx.original.method != rsip::Method::Invite
            && tx.original.method != rsip::Method::Register
        {
            return Ok(ProxyAction::Continue);
        }

        debug!(
            method = tx.original.method.to_string(),
            "Auth module processing request"
        );

        // Perform authentication
        match self.authenticate_request(tx).await {
            Ok(authenticated) => {
                if authenticated {
                    debug!("Authentication successful, continuing");
                    Ok(ProxyAction::Continue)
                } else {
                    // Extract realm from request
                    let from = if tx.original.method == rsip::Method::Register {
                        tx.original.to_header()?.uri()?.to_string()
                    } else {
                        tx.original.uri.to_string()
                    };

                    let realm = if tx.original.method == rsip::Method::Register {
                        tx.original.to_header()?.uri()?.host().to_string()
                    } else {
                        tx.original.uri.host().to_string()
                    };
                    let realm = ProxyConfig::normalize_realm(&realm);
                    // Check which type of authentication was attempted or send both challenges
                    let has_proxy_auth_header =
                        rsip::header_opt!(tx.original.headers.iter(), Header::ProxyAuthorization)
                            .is_some();
                    if has_proxy_auth_header {
                        // Send proxy challenge if proxy auth was attempted
                        let proxy_auth = self.create_proxy_auth_challenge(&realm)?;
                        info!(
                            from,
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
                            from,
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
            }
            Err(e) => {
                debug!(error = e.to_string(), "Authentication error");
                Err(e)
            }
        }
    }
}
