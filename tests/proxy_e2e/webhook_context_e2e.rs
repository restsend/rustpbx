//! Real-SIP-call RWI webhook context E2E: pins the per-call context
//! contract over a REAL INVITE → answer → hangup flow, delivered through
//! the actual `[rwi_webhook]` HTTP POST path:
//! 1. `call_created.sip_headers` carries companion headers in BOTH forms
//!    (aggregated `X-User-Data` and per-key `X-<key>`);
//! 2. `user_data` set mid-call rides every subsequent call-scoped event,
//!    including the final `call_hangup`;
//! 3. `call_answered` — emitted before user_data was set — must NOT carry
//!    the key.

use std::sync::Arc;
use std::time::Duration;

use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode, ProxyConfig};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::TestUaEvent;
use crate::common::webhook_capture::WebhookCapture;

const ALICE_SDP: &str = "v=0\r\n\
    o=- 123456 123456 IN IP4 127.0.0.1\r\n\
    s=-\r\n\
    c=IN IP4 127.0.0.1\r\n\
    t=0 0\r\n\
    m=audio 12345 RTP/AVP 0 101\r\n\
    a=rtpmap:0 PCMU/8000\r\n\
    a=rtpmap:101 telephone-event/8000\r\n\
    a=sendrecv\r\n";

const BOB_SDP: &str = "v=0\r\n\
    o=- 789012 789012 IN IP4 127.0.0.1\r\n\
    s=-\r\n\
    c=IN IP4 127.0.0.1\r\n\
    t=0 0\r\n\
    m=audio 54321 RTP/AVP 0 101\r\n\
    a=rtpmap:0 PCMU/8000\r\n\
    a=rtpmap:101 telephone-event/8000\r\n\
    a=sendrecv\r\n";

/// Poll the webhook capture until it has seen `event_type`; returns the full
/// POST envelope.
async fn wait_webhook_envelope(
    capture: &WebhookCapture,
    event_type: &str,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        {
            let received = capture.received.lock().unwrap();
            if let Some(ev) = received
                .iter()
                .find(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// A real alice→bob call whose INVITE carries companion headers in both the
/// legacy aggregated form and the per-key form. Mid-call, session user data
/// is set; the final hangup event must still carry it over the webhook.
#[tokio::test]
async fn webhook_context_sip_headers_and_user_data_full_chain() {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: vec![],
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(portpicker::pick_unused_port().unwrap_or(15060)),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        media_proxy: MediaProxyMode::All,
        ..Default::default()
    };
    config.route_originated_calls = false;
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users {
        user.is_support_webrtc = false;
    }
    let server = Arc::new(
        E2eTestServer::start_with_inject(
            config,
            E2eTestServerInject {
                users,
                rwi_gateway: Some(gateway.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("E2E server start failed"),
    );

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let bob = server.create_ua("bob").await.expect("create bob failed");

    // INVITE companion headers — both contract forms on one INVITE:
    // - legacy aggregated: `X-User-Data: k=v;k2=v2`
    // - per-key (r8): one `X-<key>` header per pair
    let extra_headers = vec![
        ("X-Tenant".to_string(), "corp_a".to_string()),
        (
            "X-User-Data".to_string(),
            "taskid=T1;crmother=C9".to_string(),
        ),
        ("X-taskid".to_string(), "T1".to_string()),
    ];

    let caller_handle = rustpbx::utils::spawn({
        let a = alice.clone();
        let sdp = ALICE_SDP.to_string();
        let headers = extra_headers.clone();
        async move { a.make_call_with_headers("bob", Some(sdp), headers).await }
    });

    // ── 1. call_created: both companion-header forms must surface ──
    let created = wait_webhook_envelope(&capture, "call_created")
        .await
        .expect("webhook must receive call_created for the inbound INVITE");
    assert_eq!(created["rwi"], "1.0", "{created}");
    let session_id = created["call_id"]
        .as_str()
        .expect("call_created envelope must carry call_id")
        .to_string();
    assert_eq!(
        created["event"]["sip_headers"]["X-Tenant"], "corp_a",
        "whitelisted custom header missing from call_created.sip_headers: {created}"
    );
    assert_eq!(
        created["event"]["sip_headers"]["X-User-Data"], "taskid=T1;crmother=C9",
        "legacy aggregated X-User-Data form must ride call_created.sip_headers: {created}"
    );
    assert_eq!(
        created["event"]["sip_headers"]["X-taskid"], "T1",
        "per-key X-<key> form (r8 contract) must ride call_created.sip_headers: {created}"
    );
    // user_data was never set → the key must be absent (contract: 未设置时不出现).
    assert!(
        created["event"].get("user_data").is_none(),
        "call_created must not fabricate user_data before it is set: {created}"
    );

    // ── 2. Bob answers; the answered event must already be delivered
    //      WITHOUT user_data (it was emitted before the set). ──
    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob
            .process_dialog_events()
            .await
            .expect("process events failed");
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(BOB_SDP.to_string()))
                    .await
                    .expect("bob answer failed");
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the call");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => panic!("Call failed: {:?}", e),
        _ => panic!("Call timed out"),
    };

    let answered = wait_webhook_envelope(&capture, "call_answered")
        .await
        .expect("webhook must receive call_answered");
    assert_eq!(answered["call_id"], session_id, "{answered}");
    assert!(
        answered["event"].get("user_data").is_none(),
        "call_answered was emitted before user_data was set — it must not carry the key: {answered}"
    );

    // ── 3. Mid-call: set session user_data (the same gateway entry the REST
    //      PUT /calls/active/{id}/userdata and WS call.set_userdata use). ──
    let mut data = serde_json::Map::new();
    data.insert("crm_id".to_string(), serde_json::json!("C-1"));
    gateway
        .write()
        .set_user_data(&session_id, data)
        .expect("set_user_data must succeed for a live call");
    wait_webhook_envelope(&capture, "call_userdata_updated")
        .await
        .expect("webhook must receive call_userdata_updated");

    // ── 4. Hangup: the FINAL event must still carry user_data — pins the
    //      cleanup ordering (call_finished must not strip user_data before
    //      the hangup event is dispatched/enriched). ──
    bob.hangup(bob_dialog_id.as_ref().unwrap()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if alice
                .process_dialog_events()
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e, TestUaEvent::CallTerminated(_)))
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bob hangup must terminate alice's call");
    let _ = alice_dialog_id;

    let hangup = wait_webhook_envelope(&capture, "call_hangup")
        .await
        .expect("webhook must receive call_hangup");
    assert_eq!(hangup["call_id"], session_id, "{hangup}");
    assert_eq!(
        hangup["event"]["user_data"]["crm_id"], "C-1",
        "the FINAL call_hangup event must still carry user_data (cleanup must not race the last event): {hangup}"
    );

    server.stop();
}
