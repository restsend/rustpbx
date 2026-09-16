//! Busy-wait (camp-on) E2E tests.
//!
//! A route rule targeting a local extension carries `[busy_wait]`. When the
//! extension rejects the INVITE with 486 Busy, the caller is parked with
//! looping hold audio (183 early media) and the proxy re-dials the extension
//! until it becomes free (then connects) or the wait budget expires (then
//! rejects with the busy status).

use anyhow::Result;
use rsipstack::sip::StatusCode;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteBusyWaitConfig, RouteRule};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::RtpReceiver;
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::TestUaEvent;

fn proxy_config_with_busy_wait(max_wait_secs: u64, retry_interval_secs: u64) -> ProxyConfig {
    let mut proxy_config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        ..Default::default()
    };
    proxy_config.routes = Some(vec![RouteRule {
        name: "busy-wait-bob".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("bob".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            busy_wait: Some(RouteBusyWaitConfig {
                enabled: true,
                max_wait_secs,
                retry_interval_secs,
                hold_audio: None,
            }),
            ..Default::default()
        },
        ..Default::default()
    }]);
    proxy_config
}

async fn setup_rtp_pair() -> Result<(RtpReceiver, RtpReceiver, u16, u16)> {
    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();
    Ok((caller_receiver, callee_receiver, caller_port, callee_port))
}

async fn wait_active_calls_empty(server: &E2eTestServer, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if server.get_active_calls().is_empty() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    server.get_active_calls().is_empty()
}

/// Extension busy → caller parked → extension frees up → call connects.
#[tokio::test]
async fn test_busy_wait_connects_when_extension_becomes_free() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server =
        Arc::new(E2eTestServer::start_with_config(proxy_config_with_busy_wait(12, 1)).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let (_, _, caller_port, callee_port) = setup_rtp_pair().await?;
    let caller_sdp = make_sdp(caller_port);
    let callee_sdp = make_sdp(callee_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob: reject the first INVITE with 486, then answer the re-dial.
    let mut rejected = false;
    let mut answered = false;
    for _ in 0..100 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            match event {
                TestUaEvent::IncomingCall(id, _) if !rejected => {
                    bob.reject_call_with_reason(
                        &id,
                        Some(StatusCode::BusyHere.code()),
                        Some("Busy Here".to_string()),
                    )
                    .await?;
                    rejected = true;
                }
                TestUaEvent::IncomingCall(id, _) if !answered => {
                    bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                    answered = true;
                }
                _ => {}
            }
        }
        if answered {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(rejected, "Bob should reject the first call with 486 Busy");
    assert!(answered, "Bob should receive a re-dial and answer it");

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(
        alice_id.is_some(),
        "caller should connect after the busy extension frees up"
    );

    // Leak check: after hangup the session must clean up its registry entry.
    alice.hangup(&alice_id.unwrap()).await?;
    assert!(
        wait_active_calls_empty(&server, Duration::from_secs(5)).await,
        "no active calls should remain after hangup"
    );

    server.stop();
    Ok(())
}

/// Extension stays busy → caller kept waiting (re-dials happen) → after the
/// wait budget expires the caller is rejected with 486.
#[tokio::test]
async fn test_busy_wait_times_out_and_rejects_with_busy() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server =
        Arc::new(E2eTestServer::start_with_config(proxy_config_with_busy_wait(3, 1)).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let (_, _, caller_port, _) = setup_rtp_pair().await?;
    let caller_sdp = make_sdp(caller_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob: keep rejecting every re-dial, counting them.
    let rejecter = tokio::spawn(async move {
        let mut rejects = 0usize;
        for _ in 0..80 {
            let events = bob.process_dialog_events().await?;
            for event in events {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    bob.reject_call_with_reason(
                        &id,
                        Some(StatusCode::BusyHere.code()),
                        Some("Busy Here".to_string()),
                    )
                    .await?;
                    rejects += 1;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Ok::<usize, anyhow::Error>(rejects)
    });

    let started = Instant::now();
    let outcome = caller.await;
    let elapsed = started.elapsed();

    let make_call_result = match outcome {
        Ok(r) => r,
        Err(join_err) => panic!("caller task panicked: {join_err}"),
    };
    let err = make_call_result.expect_err("call must fail after the busy-wait budget expires");
    let err = err.to_string();
    assert!(
        err.contains("486"),
        "caller must be rejected with 486 after the wait, got: {err}"
    );
    assert!(
        elapsed >= Duration::from_secs(3),
        "caller must be kept waiting for ~max_wait_secs, gave up after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(14),
        "busy-wait must end before make_call's own 15s cap, took {elapsed:?}"
    );

    let rejects = rejecter.await??;
    assert!(
        rejects >= 2,
        "the proxy must re-dial the busy extension while waiting (got {rejects} reject(s))"
    );

    // Leak check: the session must end (not camp forever / leak the dialog).
    assert!(
        wait_active_calls_empty(&server, Duration::from_secs(5)).await,
        "no active calls should remain after the rejection"
    );

    server.stop();
    Ok(())
}

/// Regression: without a `[busy_wait]` table the busy extension rejects the
/// caller immediately (existing behaviour, busy tone then 486).
#[tokio::test]
async fn test_busy_without_config_rejects_immediately() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let (_, _, caller_port, _) = setup_rtp_pair().await?;
    let caller_sdp = make_sdp(caller_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut rejected = false;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.reject_call_with_reason(
                    &id,
                    Some(StatusCode::BusyHere.code()),
                    Some("Busy Here".to_string()),
                )
                .await?;
                rejected = true;
            }
        }
        if rejected {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(rejected, "Bob should reject the call");

    let started = Instant::now();
    let outcome = caller.await;
    let elapsed = started.elapsed();
    let make_call_result = match outcome {
        Ok(r) => r,
        Err(join_err) => panic!("caller task panicked: {join_err}"),
    };
    let err = make_call_result.expect_err("busy call must fail without busy_wait");
    assert!(err.to_string().contains("486"), "expected 486, got: {err}");
    assert!(
        elapsed < Duration::from_secs(10),
        "without busy_wait the rejection must not be delayed by a camp-on loop, took {elapsed:?}"
    );

    assert!(
        wait_active_calls_empty(&server, Duration::from_secs(5)).await,
        "no active calls should remain after the rejection"
    );

    server.stop();
    Ok(())
}

/// Caller cancels while camping on the busy extension: the session must exit
/// promptly through the caller-gone path (no deadlock, no leaked registry
/// entry) and stop re-dialing the extension.
#[tokio::test]
async fn test_busy_wait_caller_cancel_while_waiting() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server =
        Arc::new(E2eTestServer::start_with_config(proxy_config_with_busy_wait(20, 1)).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let (_, _, caller_port, _) = setup_rtp_pair().await?;
    let caller_sdp = make_sdp(caller_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob rejects the first INVITE.
    let mut rejected = false;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.reject_call_with_reason(
                    &id,
                    Some(StatusCode::BusyHere.code()),
                    Some("Busy Here".to_string()),
                )
                .await?;
                rejected = true;
            }
        }
        if rejected {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(rejected, "Bob should reject the first call with 486 Busy");

    // Alice: the busy-wait hold media arrives as 183 Session Progress —
    // capture her dialog id from the ringing event and cancel the call.
    let mut alice_dialog_id = None;
    for _ in 0..50 {
        let events = alice.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::CallRinging(id) | TestUaEvent::EarlyMedia(id) = event {
                alice_dialog_id = Some(id);
            }
        }
        if alice_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let alice_dialog_id = alice_dialog_id.expect("caller should receive the 183 hold media");
    // A pending (pre-answer) client dialog is registered in the layer under
    // its early id (no remote tag); the ringing event carries the tagged id.
    let early_id = rsipstack::dialog::DialogId {
        call_id: alice_dialog_id.call_id.clone(),
        local_tag: alice_dialog_id.local_tag.clone(),
        remote_tag: String::new(),
    };
    if alice.hangup(&early_id).await.is_err() {
        alice.hangup(&alice_dialog_id).await?;
    }

    let make_call_result = match caller.await {
        Ok(r) => r,
        Err(join_err) => panic!("caller task panicked: {join_err}"),
    };
    assert!(
        make_call_result.is_err(),
        "cancelled call must not report success"
    );

    // Deadlock / leak canary: the session must tear down and release its
    // registry entry instead of camping until the 20s budget expires.
    assert!(
        wait_active_calls_empty(&server, Duration::from_secs(5)).await,
        "session must exit promptly when the caller cancels during busy-wait"
    );

    server.stop();
    Ok(())
}
