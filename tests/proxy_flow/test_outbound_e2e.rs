use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::RtpReceiver;
use crate::common::test_helpers::{make_sdp, test_proxy_config};
use crate::common::test_ua::TestUaEvent;

#[tokio::test]
async fn test_outbound_call_establishes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sdp = make_sdp(caller_receiver.port()?);
    let callee_sdp = make_sdp(callee_receiver.port()?);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut answered = false;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                answered = true;
            }
        }
        if answered {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(answered, "Bob should answer");

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_callee_hangup_after_call() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sdp = make_sdp(caller_receiver.port()?);
    let callee_sdp = make_sdp(callee_receiver.port()?);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    sleep(Duration::from_millis(300)).await;
    bob.hangup(bob_dialog_id.as_ref().unwrap()).await.ok();
    sleep(Duration::from_millis(300)).await;

    server.stop();
    Ok(())
}

/// Outbound dial + `on_answer: enqueue` must start the real queue app on the
/// answered leg and ring the queue's agent target — the bare `QueueEnqueue`
/// bookkeeping path is not enough (no agent ringing, caller stuck on hold).
///
/// Chain under test:
///   execute_dial_core → RWI originate → bob answers → dispatch_enqueue →
///   AppStart("queue", {queue: "sip:charlie@…"}) → session queue-start →
///   QueueApp dials charlie.
///
/// Also asserts the `queue_joined` RWI event reaches the outbound SSE stream.
#[tokio::test]
async fn test_outbound_enqueue_starts_queue_and_rings_agent() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let gateway: rustpbx::rwi::RwiGatewayRef =
        Arc::new(parking_lot::RwLock::new(rustpbx::rwi::RwiGateway::new()));
    let mut cfg = test_proxy_config(15060);
    cfg.media_proxy = MediaProxyMode::All;
    let server = Arc::new(
        E2eTestServer::start_with_inject(
            cfg,
            E2eTestServerInject {
                rwi_gateway: Some(gateway.clone()),
                ..Default::default()
            },
        )
        .await?,
    );

    let bob = server.create_ua("bob").await?;
    let charlie = server.create_ua("charlie").await?;
    sleep(Duration::from_millis(200)).await;

    let ctx = rustpbx::outbound::OutboundContext {
        sip_server: server.server_ref.clone(),
        gateway,
        call_registry: server.registry.clone(),
        conference_manager: server.server_ref.conference_manager.clone(),
        http_client: reqwest::Client::new(),
        config: rustpbx::config::OutboundConfig {
            enabled: true,
            max_concurrent: 5,
            default_ring_timeout: 20,
            default_answer_timeout: 60,
            default_webhook_timeout: 2,
        },
        concurrency_limiter: Arc::new(tokio::sync::Semaphore::new(5)),
    };

    let agent_target = format!("sip:charlie@127.0.0.1:{}", charlie.local_port());
    let mut sse = rustpbx::outbound::api::execute_dial_core(
        ctx,
        rustpbx::outbound::DialRequest {
            call_id: None,
            caller_id: Some("sip:outbound@127.0.0.1".to_string()),
            destination: format!("sip:bob@127.0.0.1:{}", bob.local_port()),
            trunk: None,
            extra_headers: std::collections::HashMap::new(),
            ring_timeout: Some(20),
            on_answer: rustpbx::outbound::OnAnswer::Enqueue {
                queue: agent_target.clone(),
                priority: Some(3),
            },
            record: None,
            on_failure: None,
            metadata: std::collections::HashMap::new(),
        },
    )
    .await
    .map_err(|(status, body)| anyhow::anyhow!("dial rejected: {status} {body:?}"))?;

    // ── Bob (customer) answers the outbound call ──────────────────────────
    let bob_receiver = RtpReceiver::bind(0).await?;
    let bob_sdp = make_sdp(bob_receiver.port()?);
    let mut bob_dialog = None;
    for _ in 0..100 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(bob_sdp.clone())).await?;
                bob_dialog = Some(id);
            }
        }
        if bob_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let bob_dialog = bob_dialog.expect("Bob should receive the outbound call");
    assert!(bob_dialog.to_string().len() > 0);

    // ── Charlie (agent) must ring via the queue app ───────────────────────
    let charlie_receiver = RtpReceiver::bind(0).await?;
    let charlie_sdp = make_sdp(charlie_receiver.port()?);
    let mut agent_rang = None;
    for _ in 0..150 {
        let events = charlie.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                agent_rang = Some(id);
            }
        }
        if agent_rang.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let agent_dialog = agent_rang.expect("queue app must dial the agent target after enqueue");
    charlie
        .answer_call(&agent_dialog, Some(charlie_sdp))
        .await?;

    // ── The SSE stream must have carried `queue_joined` for this call ─────
    let mut saw_queue_joined = false;
    let mut saw_answered = false;
    while let Ok(entry) = sse.try_recv() {
        if entry.event == "queue_joined" {
            saw_queue_joined = true;
        }
        if entry.event == "call_answered" {
            saw_answered = true;
        }
    }
    assert!(saw_answered, "SSE stream must carry call_answered");
    assert!(
        saw_queue_joined,
        "SSE stream must carry queue_joined emitted by the session queue start"
    );

    // Let the agent leg settle, then release the customer.
    sleep(Duration::from_millis(500)).await;
    bob.hangup(&bob_dialog).await.ok();
    sleep(Duration::from_millis(300)).await;

    server.stop();
    Ok(())
}
