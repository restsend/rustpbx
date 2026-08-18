use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpReceiver, send_rtp_dtmf};
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::TestUaEvent;

#[tokio::test]
async fn test_dtmf_sip_info_caller_to_callee() -> Result<()> {
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

    // Alice sends DTMF '5' via SIP INFO
    let dtmf_body = "signal=5\nduration=160";
    alice
        .send_info(
            &alice_id.clone().unwrap(),
            "application/dtmf-relay",
            dtmf_body.as_bytes().to_vec(),
        )
        .await?;

    // Bob should receive the DTMF INFO
    let mut received_dtmf = false;
    for _ in 0..20 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::DtmfInfo(id, digit) = event {
                assert_eq!(digit, "5", "Should receive DTMF 5");
                let _ = id;
                received_dtmf = true;
            }
        }
        if received_dtmf {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(received_dtmf, "Bob should receive DTMF INFO from Alice");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_rtp_rfc2833_dtmf_telephone_event() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    let caller_sdp = make_sdp(caller_port);
    let callee_sdp = make_sdp(callee_port);
    callee_receiver.start_receiving();

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

    // Send RFC 2833 telephone-event DTMF '5' via RTP
    send_rtp_dtmf(
        std::net::SocketAddr::from(([127, 0, 0, 1], callee_port)),
        101,
        '5',
        0x11111111,
        1000,
        50000,
    )
    .await?;

    sleep(Duration::from_millis(500)).await;
    let stats = callee_receiver.get_stats().await;
    assert!(
        stats.packets_received > 0,
        "Callee should receive RTP DTMF events"
    );

    server.stop();
    Ok(())
}
