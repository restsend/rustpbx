use anyhow::Result;
use async_trait::async_trait;
use rsipstack::sip::Header;
use tracing::info;

use crate::call::{TransactionCookie, user::SipUser};
use crate::proxy::auth::{AuthBackend, AuthError};
use crate::proxy::user::UserBackend;

use super::jwt_validator::JwtValidator;

pub struct JwtAuthBackend {
    validator: JwtValidator,
    user_backend: Option<Box<dyn UserBackend>>,
    sip_header_name: String,
}

impl JwtAuthBackend {
    pub fn new(
        validator: JwtValidator,
        user_backend: Option<Box<dyn UserBackend>>,
        sip_header_name: String,
    ) -> Self {
        Self {
            validator,
            user_backend,
            sip_header_name,
        }
    }
}

#[async_trait]
impl AuthBackend for JwtAuthBackend {
    async fn authenticate(
        &self,
        original: &rsipstack::sip::Request,
        _cookie: &TransactionCookie,
    ) -> Result<Option<SipUser>, AuthError> {
        let jwt = original.headers.iter().find_map(|h| {
            if let Header::Other(name, val) = h {
                if name.eq_ignore_ascii_case(&self.sip_header_name) {
                    return Some(val.clone());
                }
            }
            None
        });

        let jwt = match jwt {
            Some(j) => j,
            None => return Ok(None),
        };

        let claims = match self.validator.validate(jwt.trim()) {
            Ok(c) => c,
            Err(e) => {
                info!(error = %e, "JWT validation failed, falling back to next auth backend");
                return Ok(None);
            }
        };

        let user_id = match self.validator.extract_user_id(&claims) {
            Some(id) => id,
            None => {
                info!("JWT valid but missing user_id claim, falling back");
                return Ok(None);
            }
        };

        let realm = original.uri().host().to_string();

        if let Some(ref ub) = self.user_backend {
            match ub.get_user(&user_id, Some(&realm), Some(original)).await {
                Ok(Some(mut user)) => {
                    if !user.enabled {
                        info!(username = %user_id, "User from JWT is disabled");
                        return Ok(None);
                    }
                    user.username = user_id;
                    return Ok(Some(user));
                }
                Ok(None) => {
                    info!(username = %user_id, "User from JWT not found in local backend");
                    return Ok(None);
                }
                Err(e) => {
                    info!(error = %e, "Error looking up JWT user in local backend");
                    return Ok(None);
                }
            }
        }

        let display_name = claims
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from);

        let user = SipUser {
            username: user_id,
            enabled: true,
            realm: Some(realm),
            display_name,
            ..Default::default()
        };

        Ok(Some(user))
    }
}
