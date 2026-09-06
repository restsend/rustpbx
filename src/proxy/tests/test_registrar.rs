use super::common::{
    create_register_request, create_test_request, create_test_server,
    create_test_server_with_config, create_test_server_with_config_and_locator, create_transaction,
};
use crate::call::{Location, TransactionCookie};
use crate::config::ProxyConfig;
use crate::proxy::locator::{Locator, LocatorStats, MemoryLocator, RealmChecker};
use crate::proxy::registrar::RegistrarModule;
use crate::proxy::{ProxyAction, ProxyModule};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::sip::Header;
use rsipstack::transport::SipAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct PausingLocator {
    inner: Arc<MemoryLocator>,
    pause_has_active: Arc<AtomicBool>,
    pause_unregister: Arc<AtomicBool>,
    checked: Arc<Notify>,
    resume: Arc<Notify>,
}

impl PausingLocator {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemoryLocator::new()),
            pause_has_active: Arc::new(AtomicBool::new(false)),
            pause_unregister: Arc::new(AtomicBool::new(false)),
            checked: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl Locator for PausingLocator {
    fn set_realm_checker(&self, checker: RealmChecker) {
        self.inner.set_realm_checker(checker);
    }

    async fn register(
        &self,
        username: &str,
        realm: Option<&str>,
        location: Location,
    ) -> Result<()> {
        self.inner.register(username, realm, location).await
    }

    async fn has_active_bindings(&self, username: &str, realm: Option<&str>) -> Result<bool> {
        let active = self.inner.has_active_bindings(username, realm).await?;
        if !active && self.pause_has_active.swap(false, Ordering::SeqCst) {
            self.checked.notify_one();
            self.resume.notified().await;
        }
        Ok(active)
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        self.inner.unregister(username, realm).await?;
        if self.pause_unregister.swap(false, Ordering::SeqCst) {
            self.checked.notify_one();
            self.resume.notified().await;
        }
        Ok(())
    }

    async fn unregister_with_address(&self, addr: &SipAddr) -> Result<Option<Vec<Location>>> {
        self.inner.unregister_with_address(addr).await
    }

    async fn lookup(&self, uri: &rsipstack::sip::Uri) -> Result<Vec<Location>> {
        self.inner.lookup(uri).await
    }

    async fn sweep_expired(&self) -> Result<Vec<Location>> {
        self.inner.sweep_expired().await
    }

    async fn online_stats(&self) -> Result<LocatorStats> {
        self.inner.online_stats().await
    }
}

#[tokio::test]
async fn test_registrar_register_success() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // Create REGISTER request
    let request = create_register_request("alice", "rustpbx.com", Some(50));

    // Create the registrar module
    let module = RegistrarModule::new(server_inner.clone(), config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test registration
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should abort after successful registration since the registrar handles it completely
    assert!(matches!(result, ProxyAction::Abort));

    // Verify that the user was registered in the locator
    let locations = server_inner
        .locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await;

    assert!(locations.is_ok());
    let locations = locations.unwrap();
    assert_eq!(locations.len(), 1);
    let location = &locations[0];
    let registered_aor = location.registered_aor.as_ref().unwrap();
    assert_eq!(registered_aor.user().unwrap_or(""), "alice");
    assert_eq!(registered_aor.host().to_string(), "rustpbx.com");
    assert!(
        location
            .contact_raw
            .as_ref()
            .unwrap()
            .contains("expires=50")
    );
    assert!(
        location.home_proxy.is_some(),
        "registrar should stamp home_proxy for clustered routing"
    );
}

#[tokio::test]
async fn test_registrar_unregister() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // First register the user
    let register_request = create_register_request("alice", "rustpbx.com", Some(60));

    let module = RegistrarModule::new(server_inner.clone(), config.clone());

    let (mut tx, _) = create_transaction(register_request).await;

    // Register the user
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Abort));

    // Now unregister by sending a REGISTER with Expires: 0
    let unregister_request = create_register_request("alice", "rustpbx.com", Some(0));

    let (mut tx, _) = create_transaction(unregister_request).await;

    // Test unregistration
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should abort after successful unregistration
    assert!(matches!(result, ProxyAction::Abort));

    // Verify that the user was unregistered
    let locations = server_inner
        .locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await;

    if let Ok(v) = locations {
        assert!(v.is_empty(), "Expected no locations after unregister")
    }
}

#[tokio::test]
async fn test_registrar_unregister_keeps_user_online_with_another_binding() {
    let mut stale_event_scenarios = Vec::new();
    for wildcard in [false, true] {
        let locator = PausingLocator::new();
        let locator_trait = Arc::new(Box::new(locator.clone()) as Box<dyn Locator>);
        let config = ProxyConfig {
            realms: Some(vec!["example.com".to_string()]),
            ..Default::default()
        };
        let (server_inner, config) =
            create_test_server_with_config_and_locator(config, locator_trait).await;
        let module = RegistrarModule::new(server_inner.clone(), config);

        let register_request = create_register_request("agent-a", "example.com", Some(60));
        let registered_aor = register_request.uri.clone();
        let (mut tx, _) = create_transaction(register_request).await;
        module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx,
                TransactionCookie::default(),
            )
            .await
            .unwrap();

        let mut events = server_inner.locator_events.as_ref().unwrap().subscribe();
        if wildcard {
            locator.pause_unregister.store(true, Ordering::SeqCst);
        } else {
            locator.pause_has_active.store(true, Ordering::SeqCst);
        }

        let mut unregister_request = create_register_request("agent-a", "example.com", Some(0));
        if wildcard {
            unregister_request
                .headers
                .retain(|header| !matches!(header, Header::Contact(_)));
            unregister_request
                .headers
                .push(Header::Other("Contact".into(), "*".into()));
        }
        let (mut unregister_tx, _) = create_transaction(unregister_request).await;
        let unregister_module = module.clone();
        let unregister_task = tokio::spawn(async move {
            unregister_module
                .on_transaction_begin(
                    CancellationToken::new(),
                    &mut unregister_tx,
                    TransactionCookie::default(),
                )
                .await
                .unwrap();
        });

        locator.checked.notified().await;

        let register_request = create_register_request("agent-a", "example.com", Some(60));
        let (mut register_tx, _) = create_transaction(register_request).await;
        let register_module = module.clone();
        let register_task = tokio::spawn(async move {
            register_module
                .on_transaction_begin(
                    CancellationToken::new(),
                    &mut register_tx,
                    TransactionCookie::default(),
                )
                .await
                .unwrap();
        });

        let first_event =
            tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
                .await
                .ok()
                .and_then(|result| result.ok());
        locator.resume.notify_one();
        unregister_task.await.unwrap();
        register_task.await.unwrap();

        let mut observed = first_event.into_iter().collect::<Vec<_>>();
        while let Ok(event) = events.try_recv() {
            observed.push(event);
        }
        if !matches!(
            observed.last(),
            Some(crate::proxy::locator::LocatorEvent::Registered(_))
        ) {
            stale_event_scenarios.push((wildcard, observed));
        }
        assert_eq!(
            server_inner
                .locator
                .lookup(&registered_aor)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    assert!(
        stale_event_scenarios.is_empty(),
        "stale unregister events followed concurrent registrations: {stale_event_scenarios:?}"
    );
}

#[tokio::test]
async fn test_registrar_with_custom_expires() {
    // Create a custom config with a different registrar_expires value
    let config = ProxyConfig {
        registrar_expires: Some(120),
        max_registrar_expires: Some(300),
        ..Default::default()
    };
    let (server_inner, config) = create_test_server_with_config(config).await;

    // Create REGISTER request with no explicit expires (should use config default)
    let request = create_register_request("alice", "rustpbx.com", None);

    // Create the registrar module
    let module = RegistrarModule::new(server_inner.clone(), config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test registration
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should abort after successful registration
    assert!(matches!(result, ProxyAction::Abort));

    // Verify that the user was registered in the locator with the custom expires value
    let locations = server_inner
        .locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await
        .unwrap();

    // Should have approximately 120 seconds expiry (from the config)
    // Actual may vary due to max-expires limits in the config
    assert!(
        locations[0].expires > 30,
        "expected expires around 120, got {}",
        locations[0].expires
    );
}

#[tokio::test]
async fn test_registrar_non_register_method() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // Create an INVITE request instead of REGISTER
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );

    // Create the registrar module
    let module = RegistrarModule::new(server_inner, config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test the module with an INVITE request
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should continue since it's not a REGISTER request
    assert!(matches!(result, ProxyAction::Continue));
}
