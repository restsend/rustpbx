use std::time::Duration;


use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};

#[tokio::test]
async fn identity_header_reaches_the_callee_untouched() {
    let server = E2eTestServer::start().await.unwrap();

    let identity = "info=<eyJhbGciOiJFUzI1NiJ9.eyJhdHQiOiJBIiwicHB0Ijoic2hha2VuIn0.c2ln>;alg=ES256;ppt=shaken";

    let mut alice = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "alice".to_string(),
        password: "password123".to_string(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27200),
        proxy_addr: server.proxy_addr,
    });
    let mut bob = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "bob".to_string(),
        password: "password456".to_string(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27201),
        proxy_addr: server.proxy_addr,
    });
    alice.start().await.unwrap();
    bob.start().await.unwrap();
    alice.register().await.unwrap();
    bob.register().await.unwrap();

    let caller_handle = tokio::spawn({
        let alice = alice.clone();
        let identity = identity.to_string();
        async move {
            alice
                .make_call_with_headers(
                    "bob",
                    None,
                    vec![
                        ("Identity".to_string(), identity),
                        ("X-Test-Control".to_string(), "ctl-1".to_string()),
                    ],
                )
                .await
        }
    });

    let mut bob_dialog = None;
    for _ in 0..600 {
        let events = bob.process_dialog_events().await.unwrap();
        for event in events {
            if let TestUaEvent::IncomingCall(id, _sdp) = event {
                bob_dialog = Some(id);
                break;
            }
        }
        if bob_dialog.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let bob_dialog = bob_dialog.expect("bob must receive the INVITE");

    let received_identity = bob
        .incoming_invite_header(&bob_dialog, "Identity")
        .await
        .expect("Identity header must survive the proxy");
    assert_eq!(received_identity, identity);
    assert_eq!(
        bob.incoming_invite_header(&bob_dialog, "X-Test-Control")
            .await
            .as_deref(),
        Some("ctl-1")
    );

    let _ = caller_handle.await;
}
