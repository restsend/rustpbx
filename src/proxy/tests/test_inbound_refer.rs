//! Inbound REFER configuration. Real subscription/lifecycle coverage lives in proxy_e2e.

use crate::config::ProxyConfig;

#[test]
fn inbound_refer_in_session_defaults_enabled() {
    let config = ProxyConfig::default();
    assert!(
        config.inbound_refer_in_session,
        "blind inbound REFERs must execute in-session by default"
    );
}

/// Queue dispatch uses a dynamic agent leg; attended REFER must preserve
/// the existing customer and consultation dialogs through the handoff.
#[tokio::test]
async fn attended_refer_from_queue_agent_preserves_existing_dialogs() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();
    use super::e2e_test_server::E2eTestServer;
    use super::test_ua::TestUaEvent;
    use crate::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    let mut config = ProxyConfig::default();
    config.media_proxy = crate::config::MediaProxyMode::All;
    config.queues.insert(
        "support".into(),
        RouteQueueConfig {
            name: Some("support".into()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "sip:bob@127.0.0.1".into(),
                    label: None,
                }],
                ..Default::default()
            },
            accept_immediately: false,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "support".into(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".into()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".into()),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let server = E2eTestServer::start_with_config(config).await.unwrap();
    for mut user in super::test_helpers::standard_test_users() {
        user.is_support_webrtc = false;
        server
            .server_ref
            .user_backend
            .create_user(user)
            .await
            .unwrap();
    }
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut calls = Vec::new();
    for (caller, callee, target) in [(&alice, &bob, "support"), (&bob, &charlie, "charlie")] {
        let caller = caller.clone();
        let offer = sdp.clone();
        let dial =
            crate::utils::spawn(
                async move { caller.make_call(target, Some(offer)).await.unwrap() },
            );
        let incoming = timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        calls.push((dial.await.unwrap(), incoming));
    }
    bob.send_reinvite(&calls[0].1, Some(sdp.replace("sendrecv", "sendonly")))
        .await
        .unwrap();
    let consult = &calls[1].0;
    let replaces = format!(
        "{};to-tag={};from-tag={}",
        consult.call_id, consult.remote_tag, consult.local_tag
    );
    let target = format!(
        "sip:charlie@{}?Replaces={}",
        server.proxy_addr,
        urlencoding::encode(&replaces)
    );
    assert_eq!(bob.send_refer(&calls[0].1, &target).await.unwrap(), 202);
    timeout(Duration::from_secs(8), async {
        loop {
            for ua in [&alice, &bob, &charlie] {
                ua.process_dialog_events().await.unwrap();
            }
            if let Some(room) = server
                .server_ref
                .conference_server
                .list_conferences_detail()
                .await
                .into_iter()
                .find(|room| room.participant_count() == 2)
            {
                assert!(room.focus_uri.is_none());
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("queue agent REFER must join the surviving dialogs");
    bob.hangup(&calls[0].1).await.unwrap();
    bob.hangup(&calls[1].0).await.unwrap();
    assert_eq!(server.registry.count(), 2);
    alice.hangup(&calls[0].0).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            charlie.process_dialog_events().await.unwrap();
            if server.registry.count() == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("customer departure must release both transferred sessions");
    server.stop();
}
