pub mod http_token_auth_backend;
pub mod jwt_auth_backend;
pub mod jwt_validator;
pub mod phone_auth;

use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AuthenticatedAgent(pub String);

pub trait TokenValidator: Send + Sync {
    fn validate_token(&self, token: &str) -> Option<String>;
}

pub type DynTokenValidator = Arc<dyn TokenValidator>;

impl<T: TokenValidator> TokenValidator for Arc<T> {
    fn validate_token(&self, token: &str) -> Option<String> {
        (**self).validate_token(token)
    }
}
