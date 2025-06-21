use super::common::{
    create_register_request, create_test_request, create_test_server,
    create_test_server_with_config, create_transaction,
};
use crate::config::ProxyConfig;
use crate::proxy::registrar::RegistrarModule;
use crate::proxy::{ProxyAction, ProxyModule};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_registrar_register_success() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // Create REGISTER request
    let request = create_register_request("alice", "example.com", Some(60));

    // Create the registrar module
    let module = RegistrarModule::new(server_inner.clone(), config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test registration
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort after successful registration since the registrar handles it completely
    assert!(matches!(result, ProxyAction::Abort));

    // Verify that the user was registered in the locator
    let locations = server_inner
        .locator
        .lookup("alice", Some("example.com"))
        .await;

    assert!(locations.is_ok());
    assert_eq!(locations.unwrap().len(), 1);
}

#[tokio::test]
async fn test_registrar_unregister() {
    // Create test server with user backend and locator
    let (server_inner, config) = create_test_server().await;

    // First register the user
    let register_request = create_register_request("alice", "example.com", Some(60));

    let module = RegistrarModule::new(server_inner.clone(), config.clone());

    let (mut tx, _) = create_transaction(register_request).await;

    // Register the user
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Abort));

    // Now unregister by sending a REGISTER with Expires: 0
    let unregister_request = create_register_request("alice", "example.com", Some(0));

    let (mut tx, _) = create_transaction(unregister_request).await;

    // Test unregistration
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort after successful unregistration
    assert!(matches!(result, ProxyAction::Abort));

    // Verify that the user was unregistered
    let locations = server_inner
        .locator
        .lookup("alice", Some("example.com"))
        .await;

    assert!(locations.is_err()); // Should return an error since the user is not registered
}

#[tokio::test]
async fn test_registrar_with_custom_expires() {
    // Create a custom config with a different registrar_expires value
    let mut config = ProxyConfig::default();
    config.registrar_expires = Some(120); // Set to 2 minutes

    // Create test server with custom config
    let (server_inner, config) = create_test_server_with_config(config).await;

    // Create REGISTER request with no explicit expires (should use config default)
    let request = create_register_request("alice", "example.com", None);

    // Create the registrar module
    let module = RegistrarModule::new(server_inner.clone(), config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test registration
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort after successful registration
    assert!(matches!(result, ProxyAction::Abort));

    // Verify that the user was registered in the locator with the custom expires value
    let locations = server_inner
        .locator
        .lookup("alice", Some("example.com"))
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
    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    // Create the registrar module
    let module = RegistrarModule::new(server_inner, config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test the module with an INVITE request
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since it's not a REGISTER request
    assert!(matches!(result, ProxyAction::Continue));
}
