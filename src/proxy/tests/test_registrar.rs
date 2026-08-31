use super::common::{
    create_register_request, create_test_request, create_test_server,
    create_test_server_with_config, create_transaction,
};
use crate::call::{Location, TransactionCookie};
use crate::config::ProxyConfig;
use crate::proxy::registrar::RegistrarModule;
use crate::proxy::{ProxyAction, ProxyModule};
use tokio_util::sync::CancellationToken;

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
    let config = ProxyConfig {
        realms: Some(vec!["example.com".to_string()]),
        ..Default::default()
    };
    let (server_inner, config) = create_test_server_with_config(config).await;
    let module = RegistrarModule::new(server_inner.clone(), config);

    let register_request = create_register_request("agent-a", "example.com", Some(60));
    let registered_aor = register_request.uri.clone();
    let registered_realm = registered_aor.host_with_port.to_string();
    let (mut tx, _) = create_transaction(register_request).await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    let second_aor = create_register_request("new-device", "client.invalid", None).uri;
    server_inner
        .locator
        .register(
            "agent-a",
            Some(&registered_realm),
            Location {
                aor: second_aor.clone(),
                expires: 60,
                registered_aor: Some(registered_aor.clone()),
                instance_id: Some("new-binding".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let mut events = server_inner.locator_events.as_ref().unwrap().subscribe();
    let unregister_request = create_register_request("agent-a", "example.com", Some(0));
    let (mut tx, _) = create_transaction(unregister_request).await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    let locations = server_inner.locator.lookup(&registered_aor).await.unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].aor, second_aor);
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
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
