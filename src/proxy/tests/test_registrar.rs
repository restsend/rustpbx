use super::common::{
    create_register_request, create_test_request, create_test_server,
    create_test_server_with_config, create_transaction,
};
use crate::call::TransactionCookie;
use crate::config::ProxyConfig;
use crate::proxy::registrar::RegistrarModule;
use crate::proxy::{ProxyAction, ProxyModule};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_registrar_register_success() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // Create REGISTER request
    let request = create_register_request("alice", "rustpbx.com", Some(60));

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
            .contains("expires=60")
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

    match locations {
        Ok(v) => assert!(v.is_empty(), "Expected no locations after unregister"),
        Err(_) => {}
    }
}

#[tokio::test]
async fn test_registrar_with_custom_expires() {
    // Create a custom config with a different registrar_expires value
    let mut config = ProxyConfig::default();
    config.registrar_expires = Some(120); // Set to 2 minutes

    // Create test server with custom config
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

    // Should have 120 seconds expiry (from the config)
    assert_eq!(locations[0].expires, 120);
}

#[tokio::test]
async fn test_registrar_non_register_method() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // Create an INVITE request instead of REGISTER
    let request = create_test_request(rsip::Method::Invite, "alice", None, "rustpbx.com", None);

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
