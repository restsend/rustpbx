use super::{server::SipServerRef, user::SipUser, ProxyAction, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rsip::headers::auth::Algorithm;
use rsip::headers::ProxyAuthenticate as ProxyAuthenticateHeader;
use rsip::headers::UntypedHeader;
use rsip::headers::WwwAuthenticate as WwwAuthenticateHeader;
use rsip::prelude::HeadersExt;
use rsip::prelude::ToTypedHeader;
use rsip::services::DigestGenerator;
use rsip::typed::WwwAuthenticate;
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
        let auth_header = match tx.original.www_authenticate_header() {
            Some(h) => Header::WwwAuthenticate(h.clone()),
            None => {
                let proxy_header =
                    rsip::header_opt!(tx.original.headers.iter(), Header::ProxyAuthenticate);
                match proxy_header {
                    Some(h) => Header::ProxyAuthenticate(h.clone()),
                    None => return Ok(false),
                }
            }
        };

        // Extract user information from request
        let user = match SipUser::try_from(&tx.original) {
            Ok(user) => user,
            Err(e) => {
                debug!("Failed to extract user from request: {}", e);
                return Err(anyhow!("Failed to extract user from request: {}", e));
            }
        };

        // Get the username and realm
        let username = user.username.as_str();
        let realm = user.realm.as_deref();

        // Check if user exists and is enabled
        match self.server.user_backend.get_user(username, realm).await {
            Ok(stored_user) => {
                if !stored_user.enabled {
                    info!(username, realm, "User is disabled");
                    return Ok(false);
                }

                let challenge = match &auth_header {
                    Header::WwwAuthenticate(h) => h.typed()?,
                    Header::ProxyAuthenticate(h) => h.typed()?.0,
                    _ => return Ok(false),
                };
                let result = self.verify_credentials(
                    &stored_user,
                    &tx.original.uri,
                    &tx.original.method,
                    &challenge,
                )?;
                Ok(result)
            }
            Err(_) => {
                info!(
                    username,
                    realm, "User not found, continuing with default auth check"
                );
                return Ok(false);
            }
        }
    }

    fn verify_credentials(
        &self,
        user: &SipUser,
        uri: &Uri,
        method: &rsip::Method,
        challenge: &WwwAuthenticate,
    ) -> Result<bool> {
        Ok(true)
        // // Create a digest generator to compute the expected response
        // let expected_response = DigestGenerator {
        //     username: &user.username.as_str(),
        //     password: &user.password.as_ref().unwrap_or(""),
        //     algorithm: challenge.algorithm.unwrap_or(Algorithm::Md5),
        //     nonce: &challenge.nonce,
        //     method,
        //     qop: challenge.qop,
        //     uri,
        //     realm: &user.realm.as_deref().unwrap_or(""),
        // }
        // .compute();

        // Ok(expected_response == challenge.response)
    }

    fn create_auth_challenge(
        &self,
        realm: &str,
    ) -> Result<(WwwAuthenticateHeader, ProxyAuthenticateHeader)> {
        let nonce = rsipstack::transaction::random_text(16);
        let www_auth = WwwAuthenticateHeader::new(format!(
            r#"Digest realm="{}", nonce="{}", algorithm=MD5, qop="auth""#,
            realm, nonce
        ));

        let proxy_auth = ProxyAuthenticateHeader::new(format!(
            r#"Digest realm="{}", nonce="{}", algorithm=MD5, qop="auth""#,
            realm, nonce
        ));

        Ok((www_auth, proxy_auth))
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
                    // Extract realm from request (from To header for REGISTER, from URI for other methods)
                    let realm = if tx.original.method == rsip::Method::Register {
                        tx.original.to_header()?.uri()?.host().to_string()
                    } else {
                        tx.original.uri.host().to_string()
                    };

                    // Create challenge headers
                    let (www_auth, proxy_auth) = self.create_auth_challenge(&realm)?;

                    let headers = vec![
                        Header::WwwAuthenticate(www_auth),
                        Header::ProxyAuthenticate(proxy_auth),
                    ];
                    info!(realm, "Authentication failed, sending challenge");

                    tx.reply_with(rsip::StatusCode::Unauthorized, headers, None)
                        .await
                        .ok();
                    Ok(ProxyAction::Abort)
                }
            }
            Err(e) => {
                info!("Authentication error: {}", e);
                tx.reply(rsip::StatusCode::Unauthorized).await.ok();
                Ok(ProxyAction::Abort)
            }
        }
    }
}
