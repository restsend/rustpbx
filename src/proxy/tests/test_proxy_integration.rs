use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::config::ProxyConfig;
use crate::proxy::{
    auth::AuthModule,
    call::CallModule,
    locator::MemoryLocator,
    registrar::RegistrarModule,
    server::SipServerBuilder,
    user::{MemoryUserBackend, SipUser},
};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Create a proxy server configuration for testing
fn create_test_proxy_config(port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        tls_port: None,
        ws_port: None,
        external_ip: Some("127.0.0.1".to_string()),
        useragent: Some("RustPBX-Test/0.1.0".to_string()),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    }
}

/// Create test user data
fn create_test_users() -> Vec<SipUser> {
    vec![
        SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password123".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password456".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 3,
            username: "charlie".to_string(),
            password: Some("wrongpassword".to_string()),
            enabled: false,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 4,
            username: "david".to_string(),
            password: Some("password789".to_string()),
            enabled: true,
            realm: Some("other.com".to_string()),
            ..Default::default()
        },
    ]
}

/// Test Proxy server manager
pub struct TestProxyServer {
    cancel_token: CancellationToken,
    port: u16,
}

impl TestProxyServer {
    /// Create and start test proxy server
    pub async fn start() -> Result<Self> {
        // Use random port
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let config = Arc::new(create_test_proxy_config(port));

        // Create user backend and locator
        let user_backend = MemoryUserBackend::new(None);
        for user in create_test_users() {
            user_backend.create_user(user).await?;
        }

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();

        // Build server
        let mut builder = SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone());

        // Register modules
        builder = builder
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("auth", |inner, _config| {
                Ok(Box::new(AuthModule::new(inner)))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            });

        let server = builder.build().await?;

        // Start server
        tokio::spawn(async move {
            if let Err(e) = server.serve().await {
                warn!("Proxy server error: {:?}", e);
            }
        });

        // Wait for server startup
        sleep(Duration::from_millis(100)).await;

        info!("Test proxy server started on port {}", port);

        Ok(Self { cancel_token, port })
    }

    pub fn get_addr(&self) -> SocketAddr {
        format!("127.0.0.1:{}", self.port).parse().unwrap()
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

impl Drop for TestProxyServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Create test UA
async fn create_test_ua(
    username: &str,
    password: &str,
    proxy_addr: SocketAddr,
    port: u16,
) -> Result<TestUa> {
    let config = TestUaConfig {
        username: username.to_string(),
        password: password.to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: port,
        proxy_addr,
    };

    let mut ua = TestUa::new(config);
    ua.start().await?;
    Ok(ua)
}

#[tokio::test]
async fn test_successful_registration() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice UA
    let alice_port = portpicker::pick_unused_port().unwrap_or(25000);
    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();

    // Test registration
    assert!(
        alice.register().await.is_ok(),
        "Alice registration should succeed"
    );

    alice.stop();
    proxy.stop();
}

#[tokio::test]
async fn test_failed_registration_wrong_password() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice UA with wrong password
    let alice_port = portpicker::pick_unused_port().unwrap_or(25001);
    let alice = create_test_ua("alice", "wrongpassword", proxy_addr, alice_port)
        .await
        .unwrap();

    // Test registration failure
    assert!(
        alice.register().await.is_err(),
        "Alice registration should fail with wrong password"
    );

    alice.stop();
    proxy.stop();
}

#[tokio::test]
async fn test_failed_registration_disabled_user() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create disabled user charlie UA
    let charlie_port = portpicker::pick_unused_port().unwrap_or(25002);
    let charlie = create_test_ua("charlie", "wrongpassword", proxy_addr, charlie_port)
        .await
        .unwrap();

    // Test registration failure
    assert!(
        charlie.register().await.is_err(),
        "Charlie registration should fail (disabled user)"
    );

    charlie.stop();
    proxy.stop();
}

#[tokio::test]
async fn test_multiple_user_registration() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create multiple UAs
    let alice_port = portpicker::pick_unused_port().unwrap_or(25010);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25011);

    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    // Both users should be able to register
    assert!(
        alice.register().await.is_ok(),
        "Alice registration should succeed"
    );
    assert!(
        bob.register().await.is_ok(),
        "Bob registration should succeed"
    );

    alice.stop();
    bob.stop();
    proxy.stop();
}

#[tokio::test]
async fn test_call_success() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice and bob UAs
    let alice_port = portpicker::pick_unused_port().unwrap_or(25020);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25021);

    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    alice.register().await.unwrap();
    bob.register().await.unwrap();

    sleep(Duration::from_millis(500)).await;

    let call_task = tokio::spawn(async move { alice.make_call("bob").await });

    let answer_task = tokio::spawn(async move {
        for _ in 0..50 {
            let events = bob.process_dialog_events().await.unwrap_or_default();
            for event in events {
                match event {
                    TestUaEvent::IncomingCall(dialog_id) => {
                        info!("Bob received incoming call: {}", dialog_id);
                        bob.answer_call(&dialog_id).await.unwrap();
                        return Ok::<_, anyhow::Error>(dialog_id);
                    }
                    _ => {}
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("No incoming call received"))
    });

    let (call_result, answer_result) = tokio::join!(call_task, answer_task);

    assert!(call_result.is_ok(), "Call should be initiated successfully");
    assert!(
        answer_result.is_ok(),
        "Call should be answered successfully"
    );

    // Hang up call
    // Note: alice and bob are already moved into the spawn tasks
    // so we can't call hangup on them here

    proxy.stop();
}

#[tokio::test]
async fn test_call_to_nonexistent_user() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice UA
    let alice_port = portpicker::pick_unused_port().unwrap_or(25030);
    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();

    // Register alice
    alice.register().await.unwrap();

    // Wait for registration to take effect
    sleep(Duration::from_millis(200)).await;

    // Alice calls nonexistent user
    let result = alice.make_call("nonexistent").await;
    assert!(result.is_err(), "Call to nonexistent user should fail");

    alice.stop();
    proxy.stop();
}

#[tokio::test]
async fn test_call_to_different_realm() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice UA (test.com realm)
    let alice_port = portpicker::pick_unused_port().unwrap_or(25040);
    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();

    // Register alice
    alice.register().await.unwrap();

    // Wait for registration to take effect
    sleep(Duration::from_millis(200)).await;

    // Alice calls user in different realm (David is in other.com realm)
    let result = alice.make_call("david").await;
    assert!(
        result.is_err(),
        "Call to user in different realm should fail"
    );

    alice.stop();
    proxy.stop();
}

#[tokio::test]
async fn test_call_rejection() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice and bob UAs
    let alice_port = portpicker::pick_unused_port().unwrap_or(25050);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25051);

    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    // Register both users
    alice.register().await.unwrap();
    bob.register().await.unwrap();

    // Wait for registration to take effect
    sleep(Duration::from_millis(500)).await;

    // Alice calls Bob
    let call_task = tokio::spawn(async move { alice.make_call("bob").await });

    // Bob waits for incoming call and rejects it
    let reject_task = tokio::spawn(async move {
        for _ in 0..50 {
            let events = bob.process_dialog_events().await.unwrap_or_default();
            for event in events {
                match event {
                    TestUaEvent::IncomingCall(dialog_id) => {
                        info!("Bob received incoming call: {}", dialog_id);
                        bob.reject_call(&dialog_id).await.unwrap();
                        return Ok::<_, anyhow::Error>(());
                    }
                    _ => {}
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("No incoming call received"))
    });

    let (call_result, reject_result) = tokio::join!(call_task, reject_task);

    // call_result is Ok(Result<DialogId, Error>), we need to check the inner result
    assert!(call_result.is_ok(), "Call task should complete");
    match call_result.unwrap() {
        Ok(_) => panic!("Call should be rejected, but it succeeded"),
        Err(_) => {} // This is what we expect - the call was rejected
    }
    assert!(reject_result.is_ok(), "Bob should reject the call");

    proxy.stop();
}

#[tokio::test]
async fn test_call_hangup_flow() {
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    // Create alice and bob UAs
    let alice_port = portpicker::pick_unused_port().unwrap_or(25060);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25061);

    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let mut bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    // Register both users
    alice.register().await.unwrap();
    bob.register().await.unwrap();

    // Wait for registration to take effect
    sleep(Duration::from_millis(500)).await;

    // Alice calls Bob
    let call_task = tokio::spawn(async move { alice.make_call("bob").await });

    // Bob waits for incoming call and answers it
    let answer_task = tokio::spawn(async move {
        for _ in 0..50 {
            let events = bob.process_dialog_events().await.unwrap_or_default();
            for event in events {
                match event {
                    TestUaEvent::IncomingCall(dialog_id) => {
                        info!("Bob received incoming call: {}", dialog_id);
                        bob.answer_call(&dialog_id).await.unwrap();

                        // Wait for a while after answering, then hang up
                        sleep(Duration::from_millis(500)).await;
                        bob.hangup(&dialog_id).await.unwrap();
                        return Ok::<_, anyhow::Error>(dialog_id);
                    }
                    _ => {}
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("No incoming call received"))
    });

    let (call_result, answer_result) = tokio::join!(call_task, answer_task);

    assert!(call_result.is_ok(), "Call should be established");
    assert!(answer_result.is_ok(), "Call should be answered and hung up");

    proxy.stop();
}
