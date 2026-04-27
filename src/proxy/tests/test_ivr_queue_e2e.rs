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
    server::{SipServer, SipServerBuilder},
    user::MemoryUserBackend,
};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{Level, info, warn};

/// Helper function: Create ProxyConfig with IVR and queue configuration
fn create_ivr_queue_proxy_config(port: u16, ivr_toml_path: &str) -> ProxyConfig {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        // This test verifies the REFER-based transfer flow explicitly.
        blind_transfer_use_refer: true,
        ..Default::default()
    };

    // Queue config
    let target_uri = format!("sip:agent@127.0.0.1:{}", port);
    let queue_config = RouteQueueConfig {
        name: Some("support".to_string()),
        strategy: RouteQueueStrategyConfig {
            targets: vec![RouteQueueTargetConfig {
                uri: target_uri,
                label: Some("Support Agent".to_string()),
            }],
            ..Default::default()
        },
        accept_immediately: false,
        ..Default::default()
    };
    config.queues.insert("support".to_string(), queue_config);

    // Route 1: dial "ivr" -> run IVR app
    let ivr_route = RouteRule {
        name: "route_to_ivr".to_string(),
        priority: 5,
        match_conditions: MatchConditions {
            to_user: Some("ivr".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": ivr_toml_path,
            })),
            auto_answer: true,
            ..Default::default()
        },
        ..Default::default()
    };

    // Route 2: dial "support" -> queue
    let queue_route = RouteRule {
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

    config.routes = Some(vec![ivr_route, queue_route]);
    config
}

struct TestIvrQueueServer {
    cancel_token: CancellationToken,
    port: u16,
    server: Arc<crate::proxy::server::SipServerInner>,
}

impl TestIvrQueueServer {
    async fn start(ivr_toml_path: &str) -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let config = Arc::new(create_ivr_queue_proxy_config(port, ivr_toml_path));

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
                Ok(Box::new(AuthModule::new(
                    inner.clone(),
                    inner.proxy_config.clone(),
                )))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            });

        let server: SipServer = builder.build().await?;
        let inner = server.inner.clone();

        tokio::spawn(async move {
            if let Err(e) = server.serve().await {
                warn!("Server error: {:?}", e);
            }
        });
        sleep(Duration::from_millis(200)).await;

        Ok(Self {
            cancel_token,
            port,
            server: inner,
        })
    }

    fn get_addr(&self) -> std::net::SocketAddr {
        format!("127.0.0.1:{}", self.port).parse().unwrap()
    }
}

impl Drop for TestIvrQueueServer {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

fn create_test_ivr_toml(path: &str) {
    let toml = r#"
[ivr]
name = "test-ivr"
lang = "en"

[ivr.root]
greeting = "sounds/welcome.wav"
timeout_ms = 5000
max_retries = 1

[[ivr.root.entries]]
key = "1"
label = "Sales"
action = { type = "transfer", target = "2001" }

[[ivr.root.entries]]
key = "2"
label = "Support"
action = { type = "transfer", target = "support" }
"#;
    std::fs::write(path, toml).expect("failed to write IVR toml");
}

#[tokio::test]
async fn test_ivr_queue_e2e() {
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::DEBUG)
        .try_init()
        .ok();

    let temp_dir = std::env::temp_dir().join("rustpbx_ivr_queue_test");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let ivr_toml = temp_dir.join("ivr.toml").to_string_lossy().to_string();
    create_test_ivr_toml(&ivr_toml);

    // Ensure welcome wav exists (create a minimal one if needed)
    let sounds_dir = temp_dir.join("sounds");
    std::fs::create_dir_all(&sounds_dir).unwrap();
    let welcome_wav = sounds_dir.join("welcome.wav");
    create_minimal_wav(&welcome_wav);

    // Rewrite IVR toml with absolute path for greeting
    let toml = format!(
        r#"
[ivr]
name = "test-ivr"
lang = "en"

[ivr.root]
greeting = "{}"
timeout_ms = 5000
max_retries = 1

[[ivr.root.entries]]
key = "1"
label = "Sales"
action = {{ type = "transfer", target = "2001" }}

[[ivr.root.entries]]
key = "2"
label = "Support"
action = {{ type = "transfer", target = "support" }}
"#,
        welcome_wav.to_string_lossy()
    );
    std::fs::write(&ivr_toml, toml).expect("failed to rewrite IVR toml");

    // 1. Start server
    let server = TestIvrQueueServer::start(&ivr_toml).await.unwrap();
    let proxy_addr = server.get_addr();

    // 2. Register agent
    let agent_port = portpicker::pick_unused_port().unwrap_or(26000);
    let agent_config = super::test_ua::TestUaConfig {
        username: "agent".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: agent_port,
        proxy_addr,
    };
    let mut agent = TestUa::new(agent_config);
    agent.start().await.unwrap();
    agent.register().await.expect("Agent registration failed");

    // 3. Create Caller
    let caller_port = portpicker::pick_unused_port().unwrap_or(26001);
    let caller_config = super::test_ua::TestUaConfig {
        username: "caller".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: caller_port,
        proxy_addr,
    };
    let mut caller = TestUa::new(caller_config);
    caller.start().await.unwrap();

    let caller_task = tokio::spawn(async move {
        info!("Caller dialing ivr...");
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
            caller_port + 100
        );

        let dialog_id = caller.make_call("ivr", Some(sdp_offer)).await?;
        info!("Caller connected to IVR, dialog_id: {}", dialog_id);

        // Wait a bit for IVR app to start and auto-answer
        sleep(Duration::from_millis(300)).await;

        // Inject AudioComplete to simulate greeting finishing
        let registry = &server.server.active_call_registry;
        let sessions = registry.list_recent(1);
        if let Some(entry) = sessions.first()
            && let Some(handle) = registry.get_handle(&entry.session_id)
        {
            let _ = handle.send_app_event(crate::call::app::ControllerEvent::AudioComplete {
                track_id: "greeting".to_string(),
                interrupted: false,
            });
            info!("Injected AudioComplete for session {}", entry.session_id);
        }

        sleep(Duration::from_millis(200)).await;

        // Send DTMF '2' via SIP INFO
        caller.send_dtmf_info(&dialog_id, "2").await?;
        info!("Caller sent DTMF 2");

        // Wait for REFER and follow it to support
        let mut new_dialog = None;
        for _ in 0..50 {
            let events = caller.process_dialog_events().await.unwrap_or_default();
            for event in events {
                if let TestUaEvent::Referred(_, ref target) = event {
                    info!("Caller received REFER to {}", target);
                    // Parse Refer-To which may be `<sip:support>` or `sip:support`
                    let target_trimmed =
                        target.trim().trim_start_matches('<').trim_end_matches('>');
                    let target_user = target_trimmed
                        .trim_start_matches("sip:")
                        .split('@')
                        .next()
                        .unwrap_or("support");
                    new_dialog = Some(caller.make_call(target_user, None).await?);
                    info!("Caller re-invited to {}", target_user);
                }
                if let TestUaEvent::CallEstablished(ref id) = event
                    && Some(id) == new_dialog.as_ref()
                {
                    info!("Caller established queue call");
                }
            }
            if new_dialog.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        if new_dialog.is_none() {
            return Err(anyhow::anyhow!("Caller did not receive REFER"));
        }

        // Keep call alive briefly
        sleep(Duration::from_millis(800)).await;
        info!("Caller hanging up...");
        caller.hangup(new_dialog.as_ref().unwrap()).await.ok();
        Ok::<_, anyhow::Error>(())
    });

    // Agent waits for incoming call from queue
    let answer_task = tokio::spawn(async move {
        for _ in 0..60 {
            let events = agent.process_dialog_events().await.unwrap_or_default();
            for event in events {
                if let TestUaEvent::IncomingCall(dialog_id, _) = event {
                    info!("Agent received queue call: {}", dialog_id);
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
                    agent
                        .answer_call(&dialog_id, Some(sdp_answer))
                        .await
                        .unwrap();
                    sleep(Duration::from_millis(1500)).await;
                    return Ok(());
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("Agent did not receive queue call"))
    });

    let (caller_res, answer_res) = tokio::join!(caller_task, answer_task);

    if let Err(e) = caller_res.unwrap() {
        panic!("Caller flow failed: {:?}", e);
    }
    if let Err(e) = answer_res.unwrap() {
        panic!("Agent flow failed: {:?}", e);
    }
}

fn create_minimal_wav(path: &std::path::Path) {
    // Minimal valid WAV header for a silent 0.1s mono 8kHz PCM file
    let sample_rate = 8000u32;
    let duration_sec = 0.1f32;
    let num_samples = (sample_rate as f32 * duration_sec) as u32;
    let data_size = num_samples * 2; // 16-bit mono
    let file_size = 36 + data_size;

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // Subchunk1Size
    wav.extend_from_slice(&1u16.to_le_bytes()); // AudioFormat (PCM)
    wav.extend_from_slice(&1u16.to_le_bytes()); // NumChannels
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // ByteRate
    wav.extend_from_slice(&2u16.to_le_bytes()); // BlockAlign
    wav.extend_from_slice(&16u16.to_le_bytes()); // BitsPerSample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend(std::iter::repeat_n(0u8, data_size as usize));
    std::fs::write(path, wav).expect("failed to write wav");
}
