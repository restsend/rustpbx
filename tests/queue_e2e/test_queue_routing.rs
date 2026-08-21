use anyhow::Result;
use async_trait::async_trait;
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
use rustpbx::proxy::routing::{
    MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};

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
        accept_immediately: false,
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

#[derive(Clone)]
struct QueueTestHook {
    connected: Arc<Mutex<Vec<CallSessionContext>>>,
}

#[async_trait]
impl CallSessionHook for QueueTestHook {
    async fn on_call_connected(&self, ctx: &CallSessionContext) {
        self.connected.lock().await.push(ctx.clone());
    }

    async fn on_call_ended(
        &self,
        _ctx: &CallSessionContext,
        _reason: Option<&rustpbx::callrecord::CallRecordHangupReason>,
        _duration_secs: u64,
    ) {
    }
}

#[tokio::test]
async fn test_call_queue_routing_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let connected: Arc<Mutex<Vec<CallSessionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn CallSessionHook> = Arc::new(QueueTestHook {
        connected: connected.clone(),
    });

    let server = E2eTestServer::start_with_inject(
        create_queue_proxy_config(portpicker::pick_unused_port().unwrap_or(15060)),
        E2eTestServerInject {
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
            session_hook: Some(hook),
            agent_registry: None,
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

    let call_task = tokio::spawn({
        let c = caller;
        async move {
            let dialog_id = c.make_call("support", Some(sdp_offer)).await?;
            sleep(Duration::from_millis(500)).await;
            c.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        }
    });

    let mut agent_dialog_id = None;
    for _ in 0..50 {
        let events = agent.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                let sdp_answer = "v=0\r\n\
                    o=agent 2 0 IN IP4 127.0.0.1\r\ns=agent\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                    m=audio 30002 RTP/AVP 0 101\r\n\
                    a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
                    .to_string();
                agent.answer_call(&id, Some(sdp_answer)).await?;
                agent_dialog_id = Some(id.clone());
                break;
            }
        }
        if agent_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        agent_dialog_id.is_some(),
        "Agent should receive queued call"
    );

    let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
    sleep(Duration::from_millis(500)).await;

    let connected_events = connected.lock().await;
    assert!(
        !connected_events.is_empty(),
        "on_call_connected should have fired"
    );
    assert!(
        connected_events[0].callee.contains("support"),
        "callee should contain 'support'"
    );

    server.stop();
    Ok(())
}
