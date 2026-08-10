use super::test_helpers;
use super::test_ua::{TestUa, TestUaEvent};
use crate::call::user::SipUser;
use crate::config::ProxyConfig;
use crate::proxy::{
    locator::MemoryLocator,
    proxy_call::session_hooks::{CallSessionContext, CallSessionHook},
    routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    },
    server::SipServerBuilder,
    user::MemoryUserBackend,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{Level, info, warn};

// Helper function: Create ProxyConfig with queue configuration
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

    // 1. Define queue "support"
    // Strategy: Sequential ringing, target is sip:agent@127.0.0.1
    let target_uri = "sip:agent@127.0.0.1".to_string();
    let queue_config = RouteQueueConfig {
        name: Some("support".to_string()),
        strategy: RouteQueueStrategyConfig {
            targets: vec![RouteQueueTargetConfig {
                uri: target_uri,
                label: Some("Support Agent".to_string()),
            }],
            ..Default::default()
        },
        accept_immediately: false, // Don't accept immediately - test basic queue flow first
        ..Default::default()
    };
    config.queues.insert("support".to_string(), queue_config);

    // 2. Define routing rule
    // When to_user is "support", route to "support" queue
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

// Helper struct: Test Server
struct TestQueueServer {
    cancel_token: CancellationToken,
    port: u16,
    events: Arc<Mutex<Vec<CallSessionContext>>>,
    ended_events: Arc<Mutex<Vec<(CallSessionContext, Option<crate::callrecord::CallRecordHangupReason>)>>>,
}

impl TestQueueServer {
    async fn start() -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let config = Arc::new(create_queue_proxy_config(port));

        // Create users: caller and agent
        let user_backend = MemoryUserBackend::new(None);
        let users = vec![
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
        ];
        for user in users {
            user_backend.create_user(user).await?;
        }

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();

        let events = Arc::new(Mutex::new(Vec::new()));
        let ended_events = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<dyn CallSessionHook> = Arc::new(QueueTestHook {
            connected: events.clone(),
            ended: ended_events.clone(),
        });

        let builder = test_helpers::register_standard_modules(
            SipServerBuilder::new(config)
                .with_user_backend(Box::new(user_backend))
                .with_locator(Box::new(locator))
                .with_cancel_token(cancel_token.clone())
                .with_session_hook(hook),
        );

        let server = builder.build().await?;

        crate::utils::spawn(async move {
            if let Err(e) = server.serve().await {
                warn!("Server error: {:?}", e);
            }
        });
        sleep(Duration::from_millis(100)).await;

        Ok(Self { cancel_token, port, events, ended_events })
    }

    fn get_addr(&self) -> std::net::SocketAddr {
        format!("127.0.0.1:{}", self.port).parse().unwrap()
    }
}

impl Drop for TestQueueServer {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

#[derive(Clone)]
struct QueueTestHook {
    connected: Arc<Mutex<Vec<CallSessionContext>>>,
    ended: Arc<Mutex<Vec<(CallSessionContext, Option<crate::callrecord::CallRecordHangupReason>)>>>,
}

#[async_trait]
impl CallSessionHook for QueueTestHook {
    async fn on_call_connected(&self, ctx: &CallSessionContext) {
        self.connected.lock().await.push(ctx.clone());
    }

    async fn on_call_ended(
        &self,
        ctx: &CallSessionContext,
        reason: Option<&crate::callrecord::CallRecordHangupReason>,
        _duration_secs: u64,
    ) {
        self.ended.lock().await.push((ctx.clone(), reason.cloned()));
    }
}

// --- Actual Test Case ---

#[tokio::test]
async fn test_call_queue_routing() {
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::DEBUG)
        .try_init()
        .ok();
    // 1. Start server
    let server = TestQueueServer::start().await.unwrap();
    let proxy_addr = server.get_addr();

    // 2. Create and register Agent
    let agent_port = portpicker::pick_unused_port().unwrap_or(26000);
    let config = crate::proxy::tests::test_ua::TestUaConfig {
        webrtc: false,
        username: "agent".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: agent_port,
        proxy_addr,
    };
    let mut agent = TestUa::new(config);
    agent.start().await.unwrap();
    agent.register().await.expect("Agent registration failed");

    // 3. Create Caller
    let caller_port = portpicker::pick_unused_port().unwrap_or(26001);
    let config = crate::proxy::tests::test_ua::TestUaConfig {
        webrtc: false,
        username: "caller".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: caller_port,
        proxy_addr,
    };
    let mut caller = TestUa::new(config);
    caller.start().await.unwrap();

    // 4. Caller dials "support" (triggers routing to queue)
    let call_task: tokio::task::JoinHandle<anyhow::Result<()>> = crate::utils::spawn(async move {
        info!("Caller dialing support...");

        // Generate a minimal SDP offer from caller
        let sdp_offer = format!(
            "v=0\r\n\
             o=caller {} 0 IN IP4 127.0.0.1\r\n\
             s=caller\r\n\
             c=IN IP4 127.0.0.1\r\n\
             t=0 0\r\n\
             m=audio {} RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=sendrecv\r\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            caller_port + 100 // Use a different port for RTP
        );

        let dialog_id = caller.make_call("support", Some(sdp_offer)).await?;
        info!("Caller connected, dialog_id: {}", dialog_id);

        // Hold the call for a short duration
        sleep(Duration::from_millis(500)).await;

        info!("Caller hanging up...");
        caller.hangup(&dialog_id).await?;
        Ok::<_, anyhow::Error>(())
    });

    // 5. Agent waits for incoming call, answers, waits for CallEstablished (ACK)
    let agent_task: tokio::task::JoinHandle<anyhow::Result<()>> = crate::utils::spawn(async move {
        let mut agent_dialog_id = None;
        for _ in 0..50 {
            let events = agent.process_dialog_events().await.unwrap_or_default();
            for event in events {
                if let TestUaEvent::IncomingCall(dialog_id, _) = event {
                    info!("Agent received call: {}", dialog_id);

                    let sdp_answer = format!(
                        "v=0\r\n\
                         o=agent {} 0 IN IP4 127.0.0.1\r\n\
                         s=agent\r\n\
                         c=IN IP4 127.0.0.1\r\n\
                         t=0 0\r\n\
                         m=audio {} RTP/AVP 0 101\r\n\
                         a=rtpmap:0 PCMU/8000\r\n\
                         a=rtpmap:101 telephone-event/8000\r\n\
                         a=sendrecv\r\n",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                        agent_port + 100
                    );
                    agent.answer_call(&dialog_id, Some(sdp_answer)).await.unwrap();
                    agent_dialog_id = Some(dialog_id);
                    break;
                }
            }
            if agent_dialog_id.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        let _agent_dialog_id = agent_dialog_id
            .ok_or_else(|| anyhow::anyhow!("Agent did not receive call"))?;
        info!("Agent answered, keeping dialog alive for teardown");

        // Wait for CallEstablished (ACK from proxy), then keep alive for BYE
        for _ in 0..50 {
            let events = agent.process_dialog_events().await.unwrap_or_default();
            if events.iter().any(|e| matches!(e, TestUaEvent::CallEstablished(_))) {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        // Let cleanup happen (caller hangs up, BYE received)
        sleep(Duration::from_millis(1500)).await;
        Ok(())
    });

    let (call_res, agent_res) = tokio::join!(call_task, agent_task);

    if let Err(e) = call_res.unwrap() {
        panic!("Call flow failed: {:?}", e);
    }
    if let Err(e) = agent_res.unwrap() {
        panic!("Agent flow failed: {:?}", e);
    }

    // Verify session hooks: on_call_connected and on_call_ended must have fired
    {
        let connected_events = server.events.lock().await;
        assert!(!connected_events.is_empty(), "on_call_connected hook should have fired");
        let connected_ctx = &connected_events[0];
        assert!(!connected_ctx.session_id.is_empty(), "session_id should be populated");
        assert!(connected_ctx.callee.contains("support"), "callee should contain 'support', got: {}", connected_ctx.callee);
    }

    // Give the session a moment to flush on_call_ended
    sleep(Duration::from_millis(500)).await;
    {
        let ended_events = server.ended_events.lock().await;
        assert!(!ended_events.is_empty(), "on_call_ended hook should have fired after caller hangup");
        let (ended_ctx, hangup_reason) = &ended_events[0];
        assert!(!ended_ctx.session_id.is_empty(), "ended session_id should be populated");
        assert!(
            matches!(
                hangup_reason,
                Some(crate::callrecord::CallRecordHangupReason::ByCaller)
                    | Some(crate::callrecord::CallRecordHangupReason::Abandoned)
            ),
            "hangup_reason should be ByCaller or Abandoned, got: {:?}", hangup_reason
        );
    }
    info!("Queue e2e verification passed: call connected and ended with caller hangup");

    // Cleanup happens automatically via Drop
}
