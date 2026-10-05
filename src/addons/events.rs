use serde::{Deserialize, Serialize};

/// Outcome classification for a SIP digest-authentication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthAttemptOutcome {
    /// Request carried no credentials at all — the normal pre-challenge
    /// leg of the digest flow. Never dispatched to addons.
    NoCredentials,
    /// Credentials verified.
    Success,
    /// User does not exist in any backend.
    UnknownUser,
    /// User exists but `enabled = false`.
    Disabled,
    /// Request realm does not match any accepted realm.
    RealmMismatch,
    /// User exists but the digest response did not verify.
    BadCredentials,
}

/// Observation of one authenticated request attempt that carried an
/// `Authorization`/`Proxy-Authorization` header. The normal
/// challenge flow (initial request without credentials → 401) is NOT
/// reported: only attempts that actually presented credentials are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthAttempt {
    pub username: String,
    pub realm: Option<String>,
    /// SIP method of the authenticated request ("REGISTER", "INVITE", …).
    pub method: String,
    /// Transport peer address ("ip:port") when available.
    pub source: Option<String>,
    pub outcome: AuthAttemptOutcome,
}

impl AuthAttemptOutcome {
    pub fn is_failure(&self) -> bool {
        !matches!(self, AuthAttemptOutcome::Success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addons::Addon;
    use crate::addons::registry::AddonRegistry;
    use std::sync::Arc;
    use std::sync::Mutex;

    struct CapturingAddon {
        attempts: Mutex<Vec<AuthAttempt>>,
    }

    impl CapturingAddon {
        fn new() -> Self {
            Self {
                attempts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Addon for CapturingAddon {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn id(&self) -> &'static str {
            "auth_observer"
        }
        fn name(&self) -> &'static str {
            "AuthObserver"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn router(&self, _state: crate::app::AppState) -> Option<axum::Router> {
            None
        }
        async fn initialize(&self, _state: crate::app::AppState) -> anyhow::Result<()> {
            Ok(())
        }
        fn on_auth_attempt(&self, attempt: &AuthAttempt) {
            self.attempts.lock().unwrap().push(attempt.clone());
        }
    }

    #[test]
    fn dispatch_reaches_all_addons() {
        let observer = Arc::new(CapturingAddon::new());
        let registry =
            AddonRegistry::with_extra_addons(vec![observer.clone() as Arc<dyn Addon>]);

        registry.dispatch_auth_attempt(&AuthAttempt {
            username: "1001".into(),
            realm: None,
            method: "REGISTER".into(),
            source: Some("1.2.3.4:5060".into()),
            outcome: AuthAttemptOutcome::BadCredentials,
        });
        registry.dispatch_auth_attempt(&AuthAttempt {
            username: "bob".into(),
            realm: Some("sip.example.com".into()),
            method: "INVITE".into(),
            source: None,
            outcome: AuthAttemptOutcome::Success,
        });

        let captured = observer.attempts.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].outcome, AuthAttemptOutcome::BadCredentials);
        assert_eq!(captured[0].source.as_deref(), Some("1.2.3.4:5060"));
        assert_eq!(captured[1].outcome, AuthAttemptOutcome::Success);
        assert_eq!(captured[1].realm.as_deref(), Some("sip.example.com"));
    }
}
