//! E2E (SP-74): answer-first sessions send the caller dialog an in-dialog
//! INFO (`application/vnd.rustpbx.event+json`,
//! `{"event":"call_bridge_connected","leg":…}`) at the real bridge moment.
//! Flow: caller → route(queue, accept_immediately) → agent leg answers →
//! caller receives exactly the bridge-connected INFO on its dialog.
//! Client contract: restsend-call `crates/restsend-proto/src/bridge_event.rs`.

use anyhow::Result;
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::routing::{
    MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};

/// The bridge-notification INFO contract (restsend-proto `bridge_event`).
const EVENT_CT: &str = "application/vnd.rustpbx.event+json";
const EVENT_NAME: &str = "call_bridge_connected";

fn create_queue_proxy_config(port: u16) -> ProxyConfig {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };

    let queue_config = RouteQueueConfig {
        name: Some("support".to_string()),
        strategy: RouteQueueStrategyConfig {
            targets: vec![RouteQueueTargetConfig {
                uri: "sip:agent@127.0.0.1".to_string(),
                label: Some("Support Agent".to_string()),
            }],
            ..Default::default()
        },
        // Answer-first: the caller leg is answered before any agent exists —
        // the exact SP-74 shape the bridge INFO exists to close.
        accept_immediately: true,
        ..Default::default()
    };
    config.queues.insert("support".to_string(), queue_config);

    let route = RouteRule {
        name: "route_to_support".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    config.routes = Some(vec![route]);

    config
}

#[tokio::test]
async fn test_queue_bridge_connected_info_on_agent_answer() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let server = E2eTestServer::start_with_inject(
        create_queue_proxy_config(portpicker::pick_unused_port().unwrap_or(15060)),
        E2eTestServerInject {
            queue_enricher: None,
            users: vec![
                SipUser {
                    id: 1,
                    username: "caller".to_string(),
                    password: Some("password".to_string()),
                    enabled: true,
                    realm: Some("127.0.0.1".to_string()),
                    ..Default::default()
                },
                SipUser {
                    id: 2,
                    username: "agent".to_string(),
                    password: Some("password".to_string()),
                    enabled: true,
                    realm: Some("127.0.0.1".to_string()),
                    ..Default::default()
                },
            ],
            session_hook: None,
            agent_registry: None,
            rwi_gateway: None,
            #[cfg(feature = "addon-cc")]
            cc_policy_db: None,
        },
    )
    .await?;
    let proxy_addr = server.proxy_addr;

    let mut agent = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "agent".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(26000),
        proxy_addr,
    });
    agent.start().await?;
    agent.register().await?;

    let mut caller = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "caller".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(26001),
        proxy_addr,
    });
    caller.start().await?;

    let sdp_offer = "v=0\r\n\
        o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio 30001 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
        .to_string();

    let agent_answered = Arc::new(AtomicBool::new(false));
    let agent_answered2 = agent_answered.clone();

    // Caller: dial the queue, then keep servicing the dialog — the bridge
    // INFO arrives on this dialog once the agent leg answers.
    let caller = Arc::new(caller);
    let caller_task = {
        let c = caller.clone();
        tokio::spawn(async move {
            let dialog_id = c.make_call("support", Some(sdp_offer)).await?;
            let mut bridge_info: Option<(String, Vec<u8>)> = None;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while tokio::time::Instant::now() < deadline {
                for event in c.process_dialog_events().await? {
                    if let TestUaEvent::InfoReceived(_id, ct, body) = event {
                        if ct.contains(EVENT_CT) && String::from_utf8_lossy(&body).contains(EVENT_NAME)
                        {
                            bridge_info = Some((ct, body));
                        }
                    }
                }
                if bridge_info.is_some() && agent_answered2.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let _ = c.hangup(&dialog_id).await;
            Ok::<_, anyhow::Error>(bridge_info)
        })
    };

    // Agent: answer the queued dispatch — that connect moment must fire the
    // caller-leg bridge INFO.
    for _ in 0..80 {
        let events = agent.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                let sdp_answer = "v=0\r\n\
                    o=agent 2 0 IN IP4 127.0.0.1\r\ns=agent\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                    m=audio 30002 RTP/AVP 0 101\r\n\
                    a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
                    .to_string();
                agent.answer_call(&id, Some(sdp_answer)).await?;
                agent_answered.store(true, Ordering::SeqCst);
                break;
            }
        }
        if agent_answered.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(agent_answered.load(Ordering::SeqCst), "agent should receive the queued call and answer");

    let bridge_info = tokio::time::timeout(Duration::from_secs(15), caller_task)
        .await
        .expect("caller task must finish")??;
    let (ct, body) = bridge_info.expect(
        "caller dialog must receive the call_bridge_connected INFO when the agent leg answers \
         (SP-74: answer-first sessions need the real-connect signal)",
    );
    assert!(ct.contains(EVENT_CT), "content type must be the rustpbx event CT: {ct}");
    let payload: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        payload.get("event").and_then(|v| v.as_str()),
        Some(EVENT_NAME),
        "payload must carry the call_bridge_connected discriminator: {payload}"
    );

    server.stop();
    Ok(())
}
