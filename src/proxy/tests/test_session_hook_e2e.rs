//! E2E tests for the `CallSessionHook` lifecycle mechanism.
//!
//! Covers:
//! - `on_call_connected` fires after both legs are connected
//! - `on_call_ended` fires during session cleanup with duration info
//! - `on_call_held` / `on_call_unheld` fires via `CallCommand::Hold/Unhold`
//! - Wholesale/trunk scenario: caller is not pre-registered; hook still fires

use super::test_helpers;
use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::call::domain::{CallCommand, LegId};
use crate::call::user::SipUser;
use crate::callrecord::CallRecordHangupReason;
use crate::config::MediaProxyMode;
use crate::proxy::routing::TrunkConfig;
use crate::proxy::{
    acl::AclModule,
    auth::AuthModule,
    call::CallModule,
    locator::MemoryLocator,
    proxy_call::session_hooks::{CallSessionContext, CallSessionHook},
    registrar::RegistrarModule,
    server::SipServerBuilder,
    user::MemoryUserBackend,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::info;

// ─── Recording hook ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum HookEvent {
    Connected(CallSessionContext),
    #[allow(dead_code)]
    Held(CallSessionContext, String),
    #[allow(dead_code)]
    Unheld(CallSessionContext, String),
    Ended(CallSessionContext, u64),
}

#[derive(Clone)]
struct RecordingHook {
    events: Arc<Mutex<Vec<HookEvent>>>,
}

impl RecordingHook {
    fn new() -> (Self, Arc<Mutex<Vec<HookEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

#[async_trait]
impl CallSessionHook for RecordingHook {
    async fn on_call_connected(&self, ctx: &CallSessionContext) {
        self.events
            .lock()
            .await
            .push(HookEvent::Connected(ctx.clone()));
    }

    async fn on_call_held(&self, ctx: &CallSessionContext, leg_id: &str) {
        self.events
            .lock()
            .await
            .push(HookEvent::Held(ctx.clone(), leg_id.to_string()));
    }

    async fn on_call_unheld(&self, ctx: &CallSessionContext, leg_id: &str) {
        self.events
            .lock()
            .await
            .push(HookEvent::Unheld(ctx.clone(), leg_id.to_string()));
    }

    async fn on_call_ended(
        &self,
        ctx: &CallSessionContext,
        _reason: Option<&CallRecordHangupReason>,
        duration_secs: u64,
    ) {
        self.events
            .lock()
            .await
            .push(HookEvent::Ended(ctx.clone(), duration_secs));
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_sdp(port: u16) -> String {
    test_helpers::pcmu_sdp("127.0.0.1", port)
}

struct TestEnv {
    port: u16,
    registry: Arc<crate::proxy::active_call_registry::ActiveProxyCallRegistry>,
    events: Arc<Mutex<Vec<HookEvent>>>,
    cancel_token: CancellationToken,
    _server_handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

async fn start_server_with_hook() -> Result<TestEnv> {
    let port = portpicker::pick_unused_port().unwrap_or(15080);
    let mut proxy_config = test_helpers::test_proxy_config(port);
    proxy_config.media_proxy = MediaProxyMode::Auto;
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    let config = Arc::new(proxy_config);

    let user_backend = MemoryUserBackend::new(None);
    for user in test_users() {
        user_backend.create_user(user).await?;
    }

    let (hook, events) = RecordingHook::new();
    let cancel_token = CancellationToken::new();

    let builder = test_helpers::register_standard_modules(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(MemoryLocator::new()))
            .with_cancel_token(cancel_token.clone())
            .with_session_hook(Arc::new(hook)),
    );

    let server = Arc::new(builder.build().await?);
    let registry = server.get_inner().active_call_registry.clone();
    let ct = cancel_token.clone();
    let _server_handle = crate::utils::spawn(async move {
        tokio::select! {
            _ = ct.cancelled() => {}
            result = server.serve() => {
                if let Err(e) = result {
                    tracing::warn!("Test server error: {:?}", e);
                }
            }
        }
    });

    sleep(Duration::from_millis(200)).await;

    Ok(TestEnv {
        port,
        registry,
        events,
        cancel_token,
        _server_handle,
    })
}

fn test_users() -> Vec<SipUser> {
    test_helpers::standard_test_users()
}

async fn create_ua(port: u16, username: &str, password: &str) -> Result<TestUa> {
    let local_port = portpicker::pick_unused_port().unwrap_or(25100);
    let proxy_addr = format!("127.0.0.1:{}", port).parse()?;
    let config = TestUaConfig {
        username: username.to_string(),
        password: password.to_string(),
        realm: "127.0.0.1".to_string(),
        local_port,
        proxy_addr,
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    ua.register().await?;
    Ok(ua)
}

/// Build a TestUa that sends calls WITHOUT registering (simulates trunk / wholesale ingress).
async fn create_unregistered_ua(port: u16, username: &str) -> Result<TestUa> {
    let local_port = portpicker::pick_unused_port().unwrap_or(25200);
    let proxy_addr = format!("127.0.0.1:{}", port).parse()?;
    let config = TestUaConfig {
        username: username.to_string(),
        password: String::new(),
        realm: "127.0.0.1".to_string(),
        local_port,
        proxy_addr,
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    // No register() call — behaves like an anonymous trunk caller
    Ok(ua)
}

/// Wait until a specific `HookEvent` variant appears, polling every 50 ms.
async fn wait_for_event<F>(
    events: &Arc<Mutex<Vec<HookEvent>>>,
    matcher: F,
    timeout: Duration,
) -> bool
where
    F: Fn(&HookEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        {
            let guard = events.lock().await;
            if guard.iter().any(&matcher) {
                return true;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Establish a call, returning (alice_dialog_id, bob_dialog_id).
async fn establish_call(
    env: &TestEnv,
    alice: &Arc<TestUa>,
    bob: &TestUa,
) -> Result<(rsipstack::dialog::DialogId, rsipstack::dialog::DialogId)> {
    let alice_sdp = make_sdp(21000);
    let bob_sdp = make_sdp(21010);

    let alice_clone = alice.clone();
    let caller_handle =
        crate::utils::spawn(async move { alice_clone.make_call("bob", Some(alice_sdp)).await });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(bob_sdp.clone())).await?;
                bob_dialog_id = Some(id);
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }

    let bob_dialog_id = bob_dialog_id.expect("Bob should receive the INVITE");
    let alice_dialog_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .expect("make_call timed out")
        .expect("task panicked")
        .expect("make_call failed");

    // Wait for registry entry
    let start = tokio::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if !env.registry.is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    sleep(Duration::from_millis(200)).await;

    info!(%alice_dialog_id, %bob_dialog_id, "Call established");
    Ok((alice_dialog_id, bob_dialog_id))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// `on_call_connected` fires once both legs are established.
#[tokio::test]
async fn test_hook_on_call_connected() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let env = start_server_with_hook().await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    sleep(Duration::from_millis(100)).await;

    let _ = establish_call(&env, &alice, &bob).await?;

    let fired = wait_for_event(
        &env.events,
        |e| matches!(e, HookEvent::Connected(_)),
        Duration::from_secs(3),
    )
    .await;
    assert!(fired, "on_call_connected hook should have fired");

    let guard = env.events.lock().await;
    let ctx = guard
        .iter()
        .find_map(|e| {
            if let HookEvent::Connected(ctx) = e {
                Some(ctx.clone())
            } else {
                None
            }
        })
        .unwrap();

    assert!(!ctx.session_id.is_empty(), "session_id should be populated");
    assert!(
        ctx.caller.contains("alice"),
        "caller should contain 'alice', got: {}",
        ctx.caller
    );
    assert!(
        ctx.callee.contains("bob"),
        "callee should contain 'bob', got: {}",
        ctx.callee
    );

    Ok(())
}

/// `on_call_ended` fires after hangup with `duration_secs >= 0`.
#[tokio::test]
async fn test_hook_on_call_ended() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let env = start_server_with_hook().await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    sleep(Duration::from_millis(100)).await;

    let (alice_id, _bob_id) = establish_call(&env, &alice, &bob).await?;

    // Let the call run for a moment so duration > 0
    sleep(Duration::from_millis(500)).await;

    // Alice hangs up
    alice.hangup(&alice_id).await?;

    let fired = wait_for_event(
        &env.events,
        |e| matches!(e, HookEvent::Ended(_, _)),
        Duration::from_secs(5),
    )
    .await;
    assert!(fired, "on_call_ended hook should have fired after hangup");

    let guard = env.events.lock().await;
    let (ctx, _duration) = guard
        .iter()
        .find_map(|e| {
            if let HookEvent::Ended(ctx, dur) = e {
                Some((ctx.clone(), *dur))
            } else {
                None
            }
        })
        .unwrap();

    assert!(!ctx.session_id.is_empty(), "session_id should be populated");

    Ok(())
}

/// `on_call_held` and `on_call_unheld` fire when `CallCommand::Hold/Unhold` is sent.
#[tokio::test]
async fn test_hook_on_hold_unhold() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let env = start_server_with_hook().await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    sleep(Duration::from_millis(100)).await;

    let _ = establish_call(&env, &alice, &bob).await?;

    // Get the session handle from the registry
    let session_id = {
        let calls = env.registry.list_recent(1);
        calls
            .into_iter()
            .next()
            .map(|e| e.session_id)
            .expect("No active session found")
    };
    info!(%session_id, "Got active session");

    let handle = env
        .registry
        .get_handle(&session_id)
        .expect("Session handle should exist");

    // Pump Alice's dialog events in background so she can respond to proxy-initiated re-INVITEs
    let alice_bg = alice.clone();
    let pump_token = CancellationToken::new();
    let pump_cancel = pump_token.clone();
    crate::utils::spawn(async move {
        loop {
            tokio::select! {
                _ = pump_cancel.cancelled() => break,
                _ = sleep(Duration::from_millis(30)) => {
                    let _ = alice_bg.process_dialog_events().await;
                }
            }
        }
    });

    // Send Hold command
    let caller_leg = LegId::new("caller");
    handle
        .send_command(CallCommand::Hold {
            leg_id: caller_leg.clone(),
            music: None,
        })
        .expect("send_command(Hold) should succeed");

    let held_fired = wait_for_event(
        &env.events,
        |e| matches!(e, HookEvent::Held(_, _)),
        Duration::from_secs(5),
    )
    .await;
    assert!(held_fired, "on_call_held hook should have fired");

    // Send Unhold command
    handle
        .send_command(CallCommand::Unhold { leg_id: caller_leg })
        .expect("send_command(Unhold) should succeed");

    let unheld_fired = wait_for_event(
        &env.events,
        |e| matches!(e, HookEvent::Unheld(_, _)),
        Duration::from_secs(5),
    )
    .await;
    assert!(unheld_fired, "on_call_unheld hook should have fired");

    pump_token.cancel();
    Ok(())
}

/// Wholesale / trunk scenario: caller is NOT registered.
/// The hook must still fire for `on_call_connected` and `on_call_ended`.
#[tokio::test]
async fn test_hook_wholesale_caller_not_registered() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Build a server that has a trunk trusting 127.0.0.1 so the unregistered
    // caller is accepted without SIP authentication.
    let port = portpicker::pick_unused_port().unwrap_or(15090);
    let mut trunks = HashMap::new();
    trunks.insert(
        "test-trunk".to_string(),
        TrunkConfig {
            dest: format!("sip:127.0.0.1:{}", port),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        },
    );

    let mut proxy_config = test_helpers::test_proxy_config(port);
    proxy_config.modules = Some(vec![
        "acl".to_string(),
        "auth".to_string(),
        "registrar".to_string(),
        "call".to_string(),
    ]);
    proxy_config.media_proxy = MediaProxyMode::Auto;
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    proxy_config.trunks = trunks;
    let config = Arc::new(proxy_config);

    let user_backend = MemoryUserBackend::new(None);
    for user in test_users() {
        user_backend.create_user(user).await?;
    }

    let (hook, events) = RecordingHook::new();
    let cancel_token = CancellationToken::new();

    let builder = SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(MemoryLocator::new()))
        .with_cancel_token(cancel_token.clone())
        .with_session_hook(Arc::new(hook))
        .register_module("acl", |inner, config| AclModule::create(inner, config))
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("auth", |inner, _config| {
            Ok(Box::new(AuthModule::new(
                inner.clone(),
                inner.proxy_config.clone(),
            )))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });

    let server = Arc::new(builder.build().await?);
    let _registry = server.get_inner().active_call_registry.clone();
    let ct = cancel_token.clone();
    let _server_handle = crate::utils::spawn(async move {
        tokio::select! {
            _ = ct.cancelled() => {}
            result = server.serve() => {
                if let Err(e) = result {
                    tracing::warn!("Test server error: {:?}", e);
                }
            }
        }
    });
    sleep(Duration::from_millis(200)).await;

    // Bob registers; "trunk" caller does not
    let bob = create_ua(port, "bob", "password456").await?;
    let trunk_caller = Arc::new(create_unregistered_ua(port, "trunk").await?);
    sleep(Duration::from_millis(100)).await;

    let trunk_sdp = make_sdp(22000);
    let bob_sdp = make_sdp(22010);

    let caller_clone = trunk_caller.clone();
    let caller_handle =
        crate::utils::spawn(async move { caller_clone.make_call("bob", Some(trunk_sdp)).await });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events_polled = bob.process_dialog_events().await?;
        for event in events_polled {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(bob_sdp.clone())).await?;
                bob_dialog_id = Some(id);
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }

    let bob_dialog_id = bob_dialog_id.expect("Bob should receive the INVITE from trunk");
    let trunk_dialog_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .expect("make_call timed out")
        .expect("task panicked")
        .expect("make_call failed");

    sleep(Duration::from_millis(500)).await;

    // Verify connected hook fires
    let connected_fired = wait_for_event(
        &events,
        |e| matches!(e, HookEvent::Connected(_)),
        Duration::from_secs(3),
    )
    .await;
    assert!(
        connected_fired,
        "on_call_connected hook should fire for unregistered (wholesale) caller"
    );

    // Hang up
    trunk_caller.hangup(&trunk_dialog_id).await?;
    let _ = bob_dialog_id;

    let ended_fired = wait_for_event(
        &events,
        |e| matches!(e, HookEvent::Ended(_, _)),
        Duration::from_secs(5),
    )
    .await;
    assert!(
        ended_fired,
        "on_call_ended hook should fire for wholesale call after hangup"
    );

    cancel_token.cancel();
    Ok(())
}
