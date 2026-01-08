use super::test_ua::{TestUa, TestUaEvent};
use crate::call::user::SipUser;
use crate::config::ProxyConfig;
use crate::proxy::{
    auth::AuthModule,
    call::CallModule,
    locator::MemoryLocator,
    registrar::RegistrarModule,
    routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    },
    server::SipServerBuilder,
    user::MemoryUserBackend,
};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
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

        let mut builder = SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone());

        builder = builder
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("auth", |inner, _config| {
                Ok(Box::new(AuthModule::new(inner)))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            });

        let server = builder.build().await?;

        tokio::spawn(async move {
            if let Err(e) = server.serve().await {
                warn!("Server error: {:?}", e);
            }
        });
        sleep(Duration::from_millis(100)).await;

        Ok(Self { cancel_token, port })
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
        username: "caller".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: caller_port,
        proxy_addr,
    };
    let mut caller = TestUa::new(config);
    caller.start().await.unwrap();

    // 4. Caller dials "support" (triggers routing to queue)
    let call_task = tokio::spawn(async move {
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

    // 5. Agent waits for incoming call and answers
    let answer_task = tokio::spawn(async move {
        for _ in 0..50 {
            // Try for 5 seconds
            let events = agent.process_dialog_events().await.unwrap_or_default();
            for event in events {
                if let TestUaEvent::IncomingCall(dialog_id) = event {
                    info!("Agent received call: {}", dialog_id);

                    // Generate a minimal SDP answer
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
                        agent_port + 100 // Use a different port for RTP
                    );

                    agent
                        .answer_call(&dialog_id, Some(sdp_answer))
                        .await
                        .unwrap();

                    // Keep agent alive to receive BYE
                    sleep(Duration::from_millis(1000)).await;
                    return Ok(());
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("Agent did not receive call"))
    });

    let (call_res, answer_res) = tokio::join!(call_task, answer_task);

    if let Err(e) = call_res.unwrap() {
        panic!("Call flow failed: {:?}", e);
    }
    if let Err(e) = answer_res.unwrap() {
        panic!("Agent flow failed: {:?}", e);
    }

    // Cleanup happens automatically via Drop
}
