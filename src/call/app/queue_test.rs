//! Tests for the Queue application.
//!
//! Uses [`MockCallStack`] to drive a [`QueueApp`] through simulated events
//! without any SIP stack, media, or database.

#[cfg(test)]
mod tests {
    use crate::call::app::CallApp;
    use crate::call::app::agent_registry::AgentRegistry;
    use crate::call::app::queue::{QueueApp, QueueConfig};
    use crate::call::app::testing::MockCallStack;
    use crate::call::domain::CallCommand;
    use crate::call::{
        DialStrategy, FailureAction, Location, QueueFallbackAction, QueueHoldConfig, QueuePlan,
        VoicePrompts,
    };
    use rsipstack::sip::Uri;
    use std::time::Duration;

    /// Build a minimal queue config with a single agent for testing.
    fn build_simple_queue_config() -> QueueConfig {
        let agent_uri = Uri::try_from("sip:agent1@example.com").unwrap();
        let location = Location {
            aor: agent_uri,
            expires: 3600,
            destination: None,
            last_modified: None,
            supports_webrtc: false,
            credential: None,
            headers: None,
            registered_aor: None,
            contact_raw: None,
            contact_params: None,
            path: None,
            service_route: None,
            instance_id: None,
            gruu: None,
            temp_gruu: None,
            reg_id: None,
            transport: None,
            user_agent: None,
            home_proxy: None,
        };

        QueueConfig {
            name: "test-queue".to_string(),
            accept_immediately: true,
            hold: Some(QueueHoldConfig {
                audio_file: Some("sounds/hold_music.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                reason: Some("All agents busy".to_string()),
            })),
            agents: vec![location.clone()],
            strategy: DialStrategy::Sequential(vec![location]),
            ring_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        }
    }

    /// Build a minimal queue plan with a single agent for testing.
    fn build_simple_queue() -> QueuePlan {
        build_simple_queue_config().to_plan()
    }

    /// Build a queue config with multiple agents for sequential dialing.
    fn build_sequential_queue_config() -> QueueConfig {
        let agents: Vec<Location> = vec![
            "sip:agent1@example.com",
            "sip:agent2@example.com",
            "sip:agent3@example.com",
        ]
        .into_iter()
        .map(|uri| Location {
            aor: Uri::try_from(uri).unwrap(),
            expires: 3600,
            destination: None,
            last_modified: None,
            supports_webrtc: false,
            credential: None,
            headers: None,
            registered_aor: None,
            contact_raw: None,
            contact_params: None,
            path: None,
            service_route: None,
            instance_id: None,
            gruu: None,
            temp_gruu: None,
            reg_id: None,
            transport: None,
            user_agent: None,
            home_proxy: None,
        })
        .collect();

        QueueConfig {
            name: "sequential-queue".to_string(),
            accept_immediately: true,
            hold: Some(QueueHoldConfig {
                audio_file: Some("sounds/hold_music.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                reason: Some("All agents busy".to_string()),
            })),
            agents: agents.clone(),
            strategy: DialStrategy::Sequential(agents),
            ring_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        }
    }

    /// Build a queue plan with multiple agents for sequential dialing.
    fn build_sequential_queue() -> QueuePlan {
        build_sequential_queue_config().to_plan()
    }

    /// Build a queue config with parallel dialing.
    #[allow(dead_code)]
    fn build_parallel_queue_config() -> QueueConfig {
        let agents: Vec<Location> = vec!["sip:agent1@example.com", "sip:agent2@example.com"]
            .into_iter()
            .map(|uri| Location {
                aor: Uri::try_from(uri).unwrap(),
                expires: 3600,
                destination: None,
                last_modified: None,
                supports_webrtc: false,
                credential: None,
                headers: None,
                registered_aor: None,
                contact_raw: None,
                contact_params: None,
                path: None,
                service_route: None,
                instance_id: None,
                gruu: None,
                temp_gruu: None,
                reg_id: None,
                transport: None,
                user_agent: None,
                home_proxy: None,
            })
            .collect();

        QueueConfig {
            name: "parallel-queue".to_string(),
            accept_immediately: true,
            hold: Some(QueueHoldConfig {
                audio_file: Some("sounds/hold_music.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                reason: Some("All agents busy".to_string()),
            })),
            agents: agents.clone(),
            strategy: DialStrategy::Parallel(agents),
            ring_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        }
    }

    // ── 1. Basic queue enter with immediate answer and hold music ──

    #[tokio::test]
    async fn test_queue_basic_enter() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Queue should answer on enter (accept_immediately = true)
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Queue should start playing hold music
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 2. Queue without immediate answer ──

    #[tokio::test]
    async fn test_queue_no_immediate_answer() {
        let mut plan = build_simple_queue();
        plan.accept_immediately = false;

        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Queue should NOT answer immediately
        // It should start hold music without answering
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 3. Queue with no agents - fallback to hangup ──

    #[tokio::test]
    async fn test_queue_no_agents_fallback() {
        let mut plan = build_simple_queue();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));

        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Queue should detect no agents and execute fallback immediately
        // No AcceptCall is sent because there are no agents to dial
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    // ── 3b. Queue with no agents and no fallback config - should return 486 busy ──

    #[tokio::test]
    async fn test_queue_no_agents_no_fallback_returns_busy() {
        let mut plan = build_simple_queue();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));
        plan.fallback = None; // No fallback configured

        let config = QueueConfig {
            name: "test-queue".to_string(),
            accept_immediately: true,
            hold: None,
            fallback: None,
            agents: vec![],
            strategy: DialStrategy::Sequential(vec![]),
            ..Default::default()
        };

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // Queue should detect no agents and return 486 Busy Here
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    // ── 4. Queue fallback with play then hangup ──

    #[tokio::test]
    async fn test_queue_play_then_hangup_fallback() {
        let mut plan = build_simple_queue();
        plan.fallback = Some(QueueFallbackAction::Failure(
            FailureAction::PlayThenHangup {
                audio_file: "sounds/all_busy.wav".to_string(),
                use_early_media: false,
                status_code: rsipstack::sip::StatusCode::TemporarilyUnavailable,
                reason: Some("All agents are busy".to_string()),
            },
        ));
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));

        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Queue detects no agents and executes fallback immediately
        // For PlayThenHangup, it currently just hangs up (play is skipped in current impl)
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    // ── 5. Queue hold music loops ──

    #[tokio::test]
    async fn test_queue_hold_music_completes() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Simulate hold music completing
        // The app calls on_audio_complete but doesn't restart the music automatically
        // It waits for external events like agent_connected
        stack.audio_complete("default");

        // App should be idle waiting for events
        tokio::time::sleep(Duration::from_millis(50)).await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 6. Remote hangup during queue ──

    #[tokio::test]
    async fn test_queue_remote_hangup() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Remote party hangs up
        stack.remote_hangup();

        stack
            .join()
            .await
            .expect("should exit cleanly on remote hangup");
    }

    // ── 7. Queue with external agent connected event ──

    #[tokio::test]
    async fn test_queue_agent_connected_event() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Simulate agent connected event
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );

        // Should connect (app exits cleanly)
        stack
            .join()
            .await
            .expect("should exit after agent connected");
    }

    // ── 8. Queue with agent busy event - retry next agent ──

    #[tokio::test]
    async fn test_queue_agent_busy_retry() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // First agent is busy
        stack.custom("agent_busy", serde_json::json!({}));
        // Auto-dials agent 2
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;

        // Should continue with next agent (no immediate action, continues waiting)
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Simulate second agent connected
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent2@example.com"}),
        );

        // Should connect (app exits cleanly)
        stack
            .join()
            .await
            .expect("should exit after agent connected");
    }

    // ── 9. Queue with all agents busy - fallback ──

    #[tokio::test]
    async fn test_queue_all_agents_busy_fallback() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // All agents are busy
        stack.custom("all_agents_busy", serde_json::json!({}));

        // Should execute fallback (hangup)
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    // ── 10. Queue with redirect fallback ──

    #[tokio::test]
    async fn test_queue_redirect_fallback() {
        let mut plan = build_simple_queue();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));
        plan.fallback = Some(QueueFallbackAction::Redirect {
            target: Uri::try_from("sip:backup@example.com").unwrap(),
        });

        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        // Queue detects no agents and executes redirect fallback
        stack
            .assert_cmd(
                200,
                "Transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "sip:backup@example.com"),
            )
            .await;

        stack.join().await.expect("should exit after redirect");
    }

    // ── 11. Queue with queue-to-queue fallback (via Transfer endpoint) ──

    #[tokio::test]
    async fn test_queue_to_queue_fallback() {
        let mut plan = build_simple_queue();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));
        plan.fallback = Some(QueueFallbackAction::Failure(FailureAction::Transfer(
            crate::call::TransferEndpoint::Queue("overflow".to_string()),
        )));

        let config = build_simple_queue_config();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // Queue detects no agents and executes queue transfer fallback
        stack
            .assert_cmd(
                200,
                "Transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "queue:overflow"),
            )
            .await;

        stack
            .join()
            .await
            .expect("should exit after queue transfer");
    }

    // ── 12. Queue with no hold music configured ──

    #[tokio::test]
    async fn test_queue_no_hold_music() {
        let mut plan = build_simple_queue();
        plan.hold = None;
        let config = build_simple_queue_config();

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // Answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Should not play any hold music, just wait
        tokio::time::sleep(Duration::from_millis(50)).await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 13. Queue app name from label ──

    #[tokio::test]
    async fn test_queue_app_name() {
        let plan = build_simple_queue();
        let config = build_simple_queue_config();
        let app = QueueApp::new(plan.clone(), config);

        assert_eq!(app.name(), "test-queue");
        assert_eq!(app.app_type(), crate::call::app::CallAppType::Queue);
    }

    // ── 14. Queue app without label uses default ──

    #[tokio::test]
    async fn test_queue_app_name_default() {
        let mut plan = build_simple_queue();
        plan.label = None;
        let config = build_simple_queue_config();
        let app = QueueApp::new(plan, config);

        assert_eq!(app.name(), "queue");
    }

    // ── 15. Queue configuration validation ──

    #[test]
    fn test_queue_config_to_plan() {
        let config = QueueConfig {
            name: "sales".to_string(),
            accept_immediately: true,
            hold: Some(crate::call::QueueHoldConfig {
                audio_file: Some("hold.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                reason: None,
            })),
            agents: vec![Location {
                aor: Uri::try_from("sip:agent@example.com").unwrap(),
                expires: 3600,
                destination: None,
                last_modified: None,
                supports_webrtc: false,
                credential: None,
                headers: None,
                registered_aor: None,
                contact_raw: None,
                contact_params: None,
                path: None,
                service_route: None,
                instance_id: None,
                gruu: None,
                temp_gruu: None,
                reg_id: None,
                transport: None,
                user_agent: None,
                home_proxy: None,
            }],
            strategy: DialStrategy::Sequential(vec![]),
            ring_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        };

        let plan = config.to_plan();
        assert_eq!(plan.label, Some("sales".to_string()));
        assert!(plan.accept_immediately);
        assert_eq!(plan.ring_timeout, Some(Duration::from_secs(60)));
    }

    // ── 16. Complex queue scenario: busy, retry, connect ──

    #[tokio::test]
    async fn test_queue_complex_scenario() {
        let plan = build_sequential_queue();
        let config = build_sequential_queue_config();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // Initial answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Hold music starts
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Agent 1 is busy
        stack.custom("agent_busy", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;

        // Agent 2 no answer
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;

        // Agent 3 connects
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent3@example.com"}),
        );

        // Hold music is stopped first.
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;

        // Should cancel agent2 leg then connect to agent 3
        stack
            .assert_cmd(200, "LegRemove-agent2", |c| {
                matches!(c, CallCommand::LegRemove { .. })
            })
            .await;
        stack
            .join()
            .await
            .expect("should exit after agent connected");
    }

    /// Test autonomous routing with DbRegistry.
    #[tokio::test]
    async fn test_autonomous_routing_with_agent_registry() {
        use crate::call::app::agent_registry::{PresenceState, RoutingStrategy, db::DbRegistry};
        use std::sync::Arc;

        // Create a DbRegistry and register an agent
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let agent_registry = Arc::new(DbRegistry::new(db));
        agent_registry
            .register(
                "agent-001".to_string(),
                "Alice".to_string(),
                "sip:agent1@example.com".to_string(),
                vec!["support".to_string()],
                1,
            )
            .await
            .unwrap();
        agent_registry
            .update_presence("agent-001", PresenceState::Idle)
            .await
            .unwrap();

        // Build queue config with autonomous routing enabled
        let mut config = build_simple_queue_config();
        config.autonomous_routing = true;
        config.skill_routing_enabled = true;
        config.required_skills = vec!["support".to_string()];
        config.routing_strategy = RoutingStrategy::LongestIdle;
        config.agents = vec![]; // No static agents, using dynamic routing
        config.strategy = DialStrategy::Sequential(vec![]);

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(agent_registry.clone());
        queue = queue.with_call_id("call-001".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");

        // Enter queue - should auto-select agent and originate call
        stack.enter().await;

        // Should answer immediately
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        // Should start hold music
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Should originate call to agent
        stack
            .assert_cmd(200, "OriginateCall", |c| {
                matches!(c, CallCommand::LegAdd { target, .. } if target == "sip:agent1@example.com")
            })
            .await;

        // Should notify external systems
        stack
            .assert_cmd(200, "NotifyEvent", |c| {
                matches!(c, CallCommand::InjectAppEvent { .. })
            })
            .await;

        // Verify agent state is ringing
        let agent = agent_registry.get_agent("agent-001").await.unwrap();
        assert!(matches!(
            agent.presence,
            PresenceState::Ringing { call_id: Some(_) }
        ));

        // Simulate agent connected
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com", "agent_id": "agent-001"}),
        );

        // Should connect (app exits cleanly)
        stack
            .join()
            .await
            .expect("should exit after agent connected");

        // Verify agent state is busy
        let agent = agent_registry.get_agent("agent-001").await.unwrap();
        assert!(matches!(
            agent.presence,
            PresenceState::Busy { call_id: None }
        ));

        // Note: no stack.join() here — agent registry checks happen after app exit
    }

    /// Test autonomous routing with no available agents.
    #[tokio::test]
    async fn test_autonomous_routing_no_agents() {
        use crate::call::app::agent_registry::db::DbRegistry;
        use std::sync::Arc;

        // Create empty DbRegistry
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let agent_registry = Arc::new(DbRegistry::new(db));

        // Build queue config with autonomous routing enabled
        let mut config = build_simple_queue_config();
        config.autonomous_routing = true;
        config.skill_routing_enabled = true;
        config.required_skills = vec!["support".to_string()];
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(agent_registry);

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");

        // Enter queue - should fallback immediately since no agents available
        stack.enter().await;

        // Should fallback (hangup) without answering first since no agents
        stack
            .assert_cmd(480, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;

        let result: anyhow::Result<()> = stack.join().await;
        result.expect("should complete successfully");
    }

    /// Test autonomous routing with all agents busy plays busy prompt before fallback.
    #[tokio::test]
    async fn test_autonomous_routing_all_agents_busy_plays_busy_prompt() {
        use crate::call::app::agent_registry::db::DbRegistry;
        use std::sync::Arc;

        // Create empty DbRegistry (no available agents)
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let agent_registry = Arc::new(DbRegistry::new(db));

        // Build queue config with autonomous routing + busy prompt configured
        let mut config = build_simple_queue_config();
        config.autonomous_routing = true;
        config.skill_routing_enabled = false;
        config.voice_prompts = Some(VoicePrompts::zh());
        config.hold = None;

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(agent_registry);

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");

        stack.enter().await;

        // Should answer the call (from accept_immediately)
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Should play the busy prompt since all agents are busy/unavailable
        stack
            .assert_cmd(200, "PlayPrompt-busy-auto", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        // Should then execute fallback (hangup)
        stack
            .assert_cmd(200, "Hangup-auto", |c| matches!(c, CallCommand::Hangup(_)))
            .await;

        stack.join().await.expect("should complete successfully");
    }

    /// Test skill routing with no resolved agents plays busy prompt before fallback.
    #[tokio::test]
    async fn test_skill_routing_no_agents_plays_busy_prompt() {
        // Build queue config with skill routing enabled but no agents configured
        let mut config = build_simple_queue_config();
        config.skill_routing_enabled = true;
        config.required_skills = vec!["support".to_string()];
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.voice_prompts = Some(VoicePrompts::zh());
        config.hold = None;

        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config);

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");

        stack.enter().await;

        // Should answer the call (for busy prompt audio playback)
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Should play the busy prompt since no agents resolved
        stack
            .assert_cmd(200, "PlayPrompt-busy-skill", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        // Should then execute fallback (hangup)
        stack
            .assert_cmd(200, "Hangup-skill", |c| matches!(c, CallCommand::Hangup(_)))
            .await;

        stack.join().await.expect("should complete successfully");
    }

    /// Test agent ring timeout handling.
    #[tokio::test]
    async fn test_agent_ring_timeout() {
        use crate::call::app::agent_registry::{PresenceState, RoutingStrategy, db::DbRegistry};
        use std::sync::Arc;

        // Create a DbRegistry and register an agent
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let agent_registry = Arc::new(DbRegistry::new(db));
        agent_registry
            .register(
                "agent-001".to_string(),
                "Alice".to_string(),
                "sip:agent1@example.com".to_string(),
                vec!["support".to_string()],
                1,
            )
            .await
            .unwrap();
        agent_registry
            .update_presence("agent-001", PresenceState::Idle)
            .await
            .unwrap();

        // Build queue config with short ring timeout
        let mut config = build_simple_queue_config();
        config.autonomous_routing = true;
        config.skill_routing_enabled = true;
        config.required_skills = vec!["support".to_string()];
        config.routing_strategy = RoutingStrategy::LongestIdle;
        config.ring_timeout = Some(Duration::from_millis(100));
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(agent_registry.clone());
        queue = queue.with_call_id("call-001".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");

        // Enter queue
        stack.enter().await;

        // Should answer and start hold music
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Should originate call
        stack
            .assert_cmd(200, "OriginateCall", |c| {
                matches!(c, CallCommand::LegAdd { target, .. } if target == "sip:agent1@example.com")
            })
            .await;

        // Wait for ring timeout
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Trigger timeout
        stack.timeout("agent_ring_timeout");

        // Drain any pending commands (the timeout handler may send multiple)
        let cmds = stack.drain_cmds();

        // Should have NotifyEvent for no-answer and Hangup
        let has_no_answer = cmds
            .iter()
            .any(|c| matches!(c, CallCommand::InjectAppEvent { .. }));
        assert!(has_no_answer, "Expected queue.agent_no_answer event");

        let has_hangup = cmds.iter().any(|c| matches!(c, CallCommand::Hangup(_)));
        assert!(has_hangup, "Expected Hangup after timeout");

        // Verify agent state is back to available
        let agent = agent_registry.get_agent("agent-001").await.unwrap();
        assert!(matches!(agent.presence, PresenceState::Idle));

        let result: anyhow::Result<()> = stack.join().await;
        result.expect("should complete successfully");
    }

    fn build_queue_config_with_prompts() -> QueueConfig {
        let mut config = build_simple_queue_config();
        config.voice_prompts = Some(VoicePrompts::zh());
        config
    }

    #[tokio::test]
    async fn test_queue_transfer_prompt() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_queue_config_with_prompts())),
            "caller",
            "1000",
        );

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt-hold", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );

        // Hold music stops when the agent answers.
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt-transfer", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .join()
            .await
            .expect("should exit after transfer prompt");
    }

    #[tokio::test]
    async fn test_queue_no_prompts_transfers_directly() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_simple_queue_config())),
            "caller",
            "1000",
        );

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );

        stack
            .join()
            .await
            .expect("should exit after agent connected");
    }

    #[tokio::test]
    async fn test_queue_busy_prompt_all_agents_busy() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_queue_config_with_prompts())),
            "caller",
            "1000",
        );

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.custom("all_agents_busy", serde_json::json!({}));

        stack
            .assert_cmd(200, "PlayPrompt-busy", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_busy_prompt_agent_exhaustion() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_queue_config_with_prompts())),
            "caller",
            "1000",
        );

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.custom("agent_busy", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_busy", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_busy", serde_json::json!({}));

        stack
            .assert_cmd(200, "PlayPrompt-busy", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_transfer_prompt_english() {
        let mut config = build_simple_queue_config();
        config.voice_prompts = Some(VoicePrompts::en());

        let plan = config.to_plan();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );

        // Hold music stops when the agent answers.
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt-en-transfer", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .join()
            .await
            .expect("should exit after english transfer prompt");
    }

    #[tokio::test]
    async fn test_queue_busy_prompt_max_wait_timeout() {
        let mut config = build_queue_config_with_prompts();
        config.max_wait_secs = 0;
        let plan = config.to_plan();

        let app = QueueApp::new(plan, config);

        let mut stack = MockCallStack::run(Box::new(app), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        stack.timeout("max_wait_timeout");

        stack
            .assert_cmd(200, "NotifyEvent", |c| {
                matches!(c, CallCommand::InjectAppEvent { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt-busy-timeout", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_no_answer_prompt_all_agents_noanswer() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_queue_config_with_prompts())),
            "caller",
            "1000",
        );

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // All agents no-answer
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_no_answer", serde_json::json!({}));

        // Should play no-answer prompt (not busy prompt)
        stack
            .assert_cmd(200, "PlayPrompt-noanswer", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_no_answer_prompt_fallback_to_busy_when_mixed() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(
            Box::new(QueueApp::new(plan, build_queue_config_with_prompts())),
            "caller",
            "1000",
        );

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Agent 1 busy, Agent 2 no-answer, Agent 3 busy
        stack.custom("agent_busy", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_busy", serde_json::json!({}));

        // Last one was busy, so should play busy prompt
        stack
            .assert_cmd(200, "PlayPrompt-busy-mixed", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_no_answer_prompt_without_config_fallsback_directly() {
        let mut config = build_simple_queue_config();
        config.voice_prompts = Some(VoicePrompts {
            no_answer_prompt: None,
            ..VoicePrompts::zh()
        });
        // Only set no_answer_prompt to None, keep transfer and busy prompts

        let plan = config.to_plan();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // All agents no-answer, no no_answer_prompt configured -> should go directly to fallback
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack.custom("agent_no_answer", serde_json::json!({}));

        // Should NOT play any prompt, just hangup
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_no_answer_prompt_ring_timeout() {
        let mut config = build_queue_config_with_prompts();
        config.max_wait_secs = 0;
        let plan = config.to_plan();

        let app = QueueApp::new(plan, config);

        let mut stack = MockCallStack::run(Box::new(app), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Ring timeout triggers no-answer path (only 1 agent in simple queue)
        stack.timeout("agent_ring_timeout");

        stack
            .assert_cmd(200, "PlayPrompt-noanswer-timeout", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        stack.audio_complete("default");

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    // ── Parallel queue: originate all agents, cancel rest on first answer ──

    #[tokio::test]
    async fn test_queue_parallel_originate_all_cancel_rest() {
        let config = build_parallel_queue_config();
        let plan = config.to_plan();
        let agents = config.agents.clone();
        assert_eq!(agents.len(), 2, "parallel queue should have 2 agents");

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // Answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Play hold music
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Should originate calls to ALL agents in parallel
        let cmd0 = stack.next_cmd(200).await.expect("LegAdd for agent1");
        let _leg_id_0 = match &cmd0 {
            CallCommand::LegAdd { leg_id, .. } => {
                leg_id.clone().expect("LegAdd should have leg_id")
            }
            _ => panic!("expected LegAdd, got {cmd0:?}"),
        };

        let cmd1 = stack.next_cmd(200).await.expect("LegAdd for agent2");
        let leg_id_1 = match &cmd1 {
            CallCommand::LegAdd { leg_id, .. } => {
                leg_id.clone().expect("LegAdd should have leg_id")
            }
            _ => panic!("expected LegAdd, got {cmd1:?}"),
        };

        // Simulate agent 1 answering first
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com", "agent_id": "agent-001"}),
        );

        // Hold music stops when the agent answers.
        let stop = stack.next_cmd(200).await.expect("StopHold");
        assert!(
            matches!(stop, CallCommand::StopPlayback { .. }),
            "expected StopPlayback after agent connected, got {stop:?}"
        );

        // Should cancel agent 2's leg via LegRemove (NOT agent 1's leg)
        let remove = stack.next_cmd(200).await.expect("LegRemove");
        match &remove {
            CallCommand::LegRemove { leg_id } => {
                assert_eq!(
                    format!("{leg_id:?}"),
                    format!("{leg_id_1:?}"),
                    "should remove the non-answering agent (agent 2)"
                );
            }
            other => panic!("expected LegRemove, got {other:?}"),
        }

        // Should exit (agent connected via LegAdd, bridge handled by SipSession)
        stack
            .join()
            .await
            .expect("should exit after agent connected (parallel)");
    }

    #[tokio::test]
    async fn test_queue_parallel_all_agents_busy_fallback() {
        let mut config = build_parallel_queue_config();
        config.fallback = Some(QueueFallbackAction::Failure(FailureAction::Hangup {
            code: Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
            reason: Some("All agents busy".to_string()),
        }));
        let plan = config.to_plan();

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // Answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        // Play hold music
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Should originate calls to both agents
        stack
            .assert_cmd(200, "LegAdd-agent1", |c| {
                matches!(c, CallCommand::LegAdd { target, .. } if target == "sip:agent1@example.com")
            })
            .await;
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { target, .. } if target == "sip:agent2@example.com")
            })
            .await;

        // Both agents fail - ring timeout
        stack.timeout("agent_ring_timeout");

        // Should hit no-answer fallback
        stack
            .assert_cmd(200, "FallbackHangup", |c| {
                matches!(c, CallCommand::Hangup(_))
            })
            .await;
    }

    #[tokio::test]
    async fn test_queue_parallel_waits_until_every_agent_fails() {
        let mut config = build_parallel_queue_config();
        config.fallback = Some(QueueFallbackAction::Failure(FailureAction::Hangup {
            code: Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
            reason: Some("All agents busy".to_string()),
        }));
        let plan = config.to_plan();

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        let first = stack.next_cmd(200).await.expect("first parallel LegAdd");
        let first_leg = match first {
            CallCommand::LegAdd {
                leg_id: Some(leg_id),
                ..
            } => leg_id,
            other => panic!("expected first LegAdd, got {other:?}"),
        };
        let second = stack.next_cmd(200).await.expect("second parallel LegAdd");
        let second_leg = match second {
            CallCommand::LegAdd {
                leg_id: Some(leg_id),
                ..
            } => leg_id,
            other => panic!("expected second LegAdd, got {other:?}"),
        };

        stack.custom("agent_busy", serde_json::json!({"leg_id": first_leg.0}));
        assert!(
            stack.next_cmd(50).await.is_none(),
            "one failed parallel agent must not trigger fallback"
        );

        stack.custom("agent_busy", serde_json::json!({"leg_id": second_leg.0}));
        stack
            .assert_cmd(200, "FallbackHangup", |c| {
                matches!(c, CallCommand::Hangup(_))
            })
            .await;
    }

    // ── 最终提示（Final Destination Prompt） ──

    #[tokio::test]
    async fn test_final_destination_prompt_no_agents_plays_prompt() {
        let mut config = QueueConfig::default(); // no agents
        config.voice_prompts = Some(VoicePrompts {
            busy_prompt: None,
            final_destination_prompt: Some("final-dest.wav".into()),
            ..VoicePrompts::zh()
        });
        let plan = config.to_plan();

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        // No agents → busy prompt is none → final destination prompt
        stack
            .assert_cmd(200, "PlayFinalPrompt", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        // Final prompt audio completes → fallback (hangup)
        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;

        stack.join().await.unwrap();
    }

    // ── 升级策略（Escalation） ──

    #[tokio::test]
    async fn test_cumulative_escalation_does_not_crash() {
        use crate::call::app::agent_registry::db::DbRegistry;
        use crate::call::app::queue::EscalationMode;
        use std::sync::Arc;

        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let registry = Arc::new(DbRegistry::new(db));
        registry
            .register(
                "agent1".into(),
                "Agent 1".into(),
                "sip:agent1@pbx".into(),
                vec!["support".into()],
                1,
            )
            .await
            .unwrap();
        registry
            .update_presence(
                "agent1",
                crate::call::app::agent_registry::PresenceState::Idle,
            )
            .await
            .unwrap();

        let mut config = build_simple_queue_config();
        config.autonomous_routing = true;
        config.skill_routing_enabled = true;
        config.required_skills = vec!["support".to_string()];
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.escalation_mode = EscalationMode::Cumulative;
        config.escalation_timeline = vec![crate::call::app::queue::EscalationStep {
            threshold_secs: 5,
            add_skill_group: "support2".to_string(),
        }];

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(registry.clone());
        queue = queue.with_call_id("call-001".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        // Trigger escalation check — should not crash even though skill-group: support2
        // doesn't resolve to any agents
        stack.timeout("escalation_check");

        stack.join().await.unwrap();
    }

    // ── Audio path resolution + action rules (online / offline scenarios) ──
    //
    // The tests below guard the regression where the queue emitted a
    // `CallCommand::Play` whose path was never resolved by `handle_play` on
    // the SipSession side, so the caller heard no hold music and no
    // transfer/busy/no-answer prompts even though the queue logic ran the
    // correct branch for the agent status.
    //
    // They verify two things together:
    //   1. The queue picks the right prompt for the agent-status branch
    //      (online → transfer; all busy → busy; all no-answer → no-answer;
    //      no agents → busy fallback).
    //   2. The emitted path actually resolves to a decodable WAV — i.e. it
    //      would reach the speaker if executed by a real SipSession.

    use crate::call::domain::MediaSource;

    /// Extract the file path from a `CallCommand::Play`, panicking otherwise.
    fn play_path(cmd: &CallCommand) -> String {
        match cmd {
            CallCommand::Play {
                source: MediaSource::File { path },
                ..
            } => path.clone(),
            other => panic!("expected CallCommand::Play with File source, got {other:?}"),
        }
    }

    /// True when the shipped `config/sounds/` tree is present (i.e. the test
    /// runs from a workspace checkout). Tests that need real audio skip
    /// otherwise — the resolution logic itself is covered in
    /// `sip_session::tests`.
    fn packaged_sounds_available() -> bool {
        std::path::Path::new("config/sounds").is_dir()
    }

    /// Resolve a `sounds/…` spec the same way `SipSession::handle_play` does,
    /// then prove the result is decodable by `FileAudioSource` — the exact
    /// gate that gates real playback.
    async fn assert_spec_is_playable(label: &str, spec: &str) {
        use crate::media::audio_source::{AudioSource, FileAudioSource};
        use crate::proxy::proxy_call::sip_session::SipSession;
        let resolved = SipSession::resolve_audio_file_path(spec);
        let src = FileAudioSource::new(resolved.clone(), false)
            .await
            .unwrap_or_else(|e| {
                panic!("[{label}] {spec} did not resolve to a playable file ({resolved}): {e}")
            });
        assert!(
            src.has_data(),
            "[{label}] resolved file decoded to zero samples: {resolved}"
        );
        let _ = AudioSource::has_data(&src);
    }

    /// Queue config wired to the *real* shipped ZH prompts so the assertion
    /// `assert_spec_is_playable` is meaningful.
    fn build_queue_config_with_default_prompts() -> QueueConfig {
        let mut config = build_simple_queue_config();
        config.hold = Some(QueueHoldConfig {
            audio_file: Some(crate::call::DEFAULT_QUEUE_HOLD_AUDIO.to_string()),
            loop_playback: true,
        });
        config.voice_prompts = Some(VoicePrompts::zh());
        config
    }

    /// Build (plan, config) where both halves reference the shipped default
    /// audio paths. The queue reads hold music from `plan.hold`, so the plan
    /// must be rebuilt from the default-wired config (not `build_simple_queue`,
    /// which carries the synthetic `sounds/hold_music.wav` fixture).
    fn build_plan_and_config_with_default_audio() -> (QueuePlan, QueueConfig) {
        let config = build_queue_config_with_default_prompts();
        let plan = config.to_plan();
        (plan, config)
    }

    #[tokio::test]
    async fn test_queue_hold_music_path_is_playable() {
        if !packaged_sounds_available() {
            eprintln!("skipping: config/sounds/ not present");
            return;
        }
        let (plan, config) = build_plan_and_config_with_default_audio();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;

        let hold_cmd = stack
            .next_cmd(200)
            .await
            .expect("expected hold-music Play command");
        let path = play_path(&hold_cmd);
        assert!(
            path.ends_with("phone-calling.wav"),
            "hold music should reference the default hold audio, got {path}"
        );
        assert_spec_is_playable("hold", &path).await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── skill-group lifecycle notify hooks ──────────────────────────────────

    /// AgentRegistry double that delegates to `MemoryRegistry` and records the
    /// skill-group lifecycle notify hooks.
    struct RecordingRegistry {
        inner: crate::call::app::agent_registry::memory::MemoryRegistry,
        abandoned: std::sync::Mutex<Vec<(String, String, u64)>>,
        timeouts: std::sync::Mutex<Vec<(String, String, u64)>>,
        fallbacks: std::sync::Mutex<Vec<(String, String, String, String)>>,
    }

    impl RecordingRegistry {
        fn new() -> Self {
            Self {
                inner: crate::call::app::agent_registry::memory::MemoryRegistry::new(),
                abandoned: std::sync::Mutex::new(Vec::new()),
                timeouts: std::sync::Mutex::new(Vec::new()),
                fallbacks: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentRegistry for RecordingRegistry {
        async fn register(
            &self,
            agent_id: String,
            display_name: String,
            uri: String,
            skills: Vec<String>,
            max_concurrency: u32,
        ) -> anyhow::Result<()> {
            self.inner
                .register(agent_id, display_name, uri, skills, max_concurrency)
                .await
        }

        async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
            self.inner.unregister(agent_id).await
        }

        async fn get_agent(
            &self,
            agent_id: &str,
        ) -> Option<crate::call::app::agent_registry::AgentRecord> {
            self.inner.get_agent(agent_id).await
        }

        async fn list_agents(&self) -> Vec<crate::call::app::agent_registry::AgentRecord> {
            self.inner.list_agents().await
        }

        async fn update_presence(
            &self,
            agent_id: &str,
            new_state: crate::call::app::agent_registry::PresenceState,
        ) -> anyhow::Result<()> {
            self.inner.update_presence(agent_id, new_state).await
        }

        async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
            self.inner.start_call(agent_id).await
        }

        async fn end_call(&self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
            self.inner.end_call(agent_id, talk_time_secs).await
        }

        async fn find_available_agents(
            &self,
            required_skills: &[String],
        ) -> Vec<crate::call::app::agent_registry::AgentRecord> {
            self.inner.find_available_agents(required_skills).await
        }

        async fn select_agent(
            &self,
            required_skills: &[String],
            strategy: crate::call::app::agent_registry::RoutingStrategy,
        ) -> Option<crate::call::app::agent_registry::AgentRecord> {
            self.inner.select_agent(required_skills, strategy).await
        }

        async fn resolve_target(&self, target_uri: &str) -> Vec<String> {
            self.inner.resolve_target(target_uri).await
        }

        async fn notify_call_abandoned(&self, call_id: &str, queue_id: &str, waited_secs: u64) {
            self.abandoned.lock().unwrap().push((
                call_id.to_string(),
                queue_id.to_string(),
                waited_secs,
            ));
        }

        async fn notify_call_timeout(&self, call_id: &str, queue_id: &str, waited_secs: u64) {
            self.timeouts.lock().unwrap().push((
                call_id.to_string(),
                queue_id.to_string(),
                waited_secs,
            ));
        }

        async fn notify_call_fallback(
            &self,
            call_id: &str,
            queue_id: &str,
            reason: &str,
            action: &str,
        ) {
            self.fallbacks.lock().unwrap().push((
                call_id.to_string(),
                queue_id.to_string(),
                reason.to_string(),
                action.to_string(),
            ));
        }
    }

    /// A skill-routed queue with no agents available must notify the dispatcher
    /// of the abandoned call and the executed fallback.
    #[tokio::test]
    async fn test_queue_skill_abandon_notifies_registry() {
        use std::sync::Arc;
        let registry = Arc::new(RecordingRegistry::new());
        let mut config = build_simple_queue_config();
        config.skill_routing_enabled = true;
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.hold = None;

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(registry.clone());
        queue = queue.with_call_id("call-001".to_string());

        let stack = MockCallStack::run(Box::new(queue), "1001", "1002");
        stack.join().await.unwrap();

        let abandoned = registry.abandoned.lock().unwrap();
        assert!(
            abandoned
                .iter()
                .any(|(c, q, _)| c == "call-001" && q == "test-queue"),
            "notify_call_abandoned must be called for an abandoned skill-routed call, got {abandoned:?}"
        );
        let fallbacks = registry.fallbacks.lock().unwrap();
        assert!(
            fallbacks
                .iter()
                .any(|(c, q, _, _)| c == "call-001" && q == "test-queue"),
            "notify_call_fallback must be called when the call could not be serviced, got {fallbacks:?}"
        );
    }

    /// A non-skill-routed queue must NOT notify the skill-group dispatcher.
    #[tokio::test]
    async fn test_queue_non_skill_routing_does_not_notify() {
        use std::sync::Arc;
        let registry = Arc::new(RecordingRegistry::new());
        let mut config = build_simple_queue_config();
        config.skill_routing_enabled = false;
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.hold = None;

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(registry.clone());
        queue = queue.with_call_id("call-001".to_string());

        let stack = MockCallStack::run(Box::new(queue), "1001", "1002");
        stack.join().await.unwrap();

        assert!(registry.abandoned.lock().unwrap().is_empty());
        assert!(registry.timeouts.lock().unwrap().is_empty());
        assert!(registry.fallbacks.lock().unwrap().is_empty());
    }

    /// Agent ring timeout must emit the `queue_agent_no_answer` RWI event via
    /// the gateway.
    #[tokio::test]
    async fn test_queue_agent_ring_timeout_emits_no_answer() {
        use crate::rwi::auth::RwiIdentity;
        use std::sync::Arc;

        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let sid = gw
            .create_session(RwiIdentity {
                token: "t".into(),
                scopes: vec![],
            })
            .read()
            .id
            .clone();
        let (gws_tx, mut gws_rx) = tokio::sync::mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, gws_tx);
        let gw = Arc::new(parking_lot::RwLock::new(gw));

        let registry = Arc::new(RecordingRegistry::new());
        registry
            .inner
            .register(
                "agent-001".to_string(),
                "Alice".to_string(),
                "sip:agent1@example.com".to_string(),
                vec!["support".to_string()],
                1,
            )
            .await
            .unwrap();
        registry
            .inner
            .update_presence(
                "agent-001",
                crate::call::app::agent_registry::PresenceState::Idle,
            )
            .await
            .unwrap();

        let mut config = build_simple_queue_config();
        config.skill_routing_enabled = true;
        config.autonomous_routing = true;
        config.required_skills = vec!["support".to_string()];
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.hold = None;

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(registry.clone());
        queue = queue.with_call_id("call-001".to_string());

        let mut ctx = crate::call::app::ApplicationContext::new(
            sea_orm::DatabaseConnection::default(),
            crate::call::app::CallInfo {
                session_id: "test-session".into(),
                caller: "1001".into(),
                callee: "1002".into(),
                direction: "inbound".into(),
                started_at: chrono::Utc::now(),
                sip_headers: Default::default(),
                route_name: None,
            },
            Arc::new(crate::config::Config::default()),
        );
        ctx.rwi_gateway = Some(gw);

        let mut stack = MockCallStack::run_with_context(Box::new(queue), ctx);
        stack.enter().await;

        // accept_immediately → Answer first, then autonomous dial of the agent.
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "OriginateCall", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.timeout("agent_ring_timeout");

        // The no-answer event is broadcast synchronously from the handler; poll briefly.
        let mut saw = false;
        for _ in 0..20 {
            while let Ok(v) = gws_rx.try_recv() {
                if v.get("event_type").and_then(|e| e.as_str()) == Some("queue_agent_no_answer") {
                    saw = true;
                }
            }
            if saw {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            saw,
            "queue_agent_no_answer event must be emitted on ring timeout"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    #[tokio::test]
    async fn test_queue_online_agent_plays_transfer_prompt_that_is_playable() {
        if !packaged_sounds_available() {
            eprintln!("skipping: config/sounds/ not present");
            return;
        }
        let (plan, config) = build_plan_and_config_with_default_audio();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        // discard hold music
        let _ = stack.next_cmd(200).await.expect("hold music");

        // Simulate an agent coming online and accepting the call.
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );

        // Hold music is stopped first.
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;

        let transfer_cmd = stack
            .next_cmd(200)
            .await
            .expect("expected transfer-prompt Play command");
        let path = play_path(&transfer_cmd);
        assert!(
            path.ends_with("queue-transfer-zh.wav"),
            "online agent should trigger the ZH transfer prompt, got {path}"
        );
        assert_spec_is_playable("transfer", &path).await;

        stack.audio_complete("default");
        let _ = stack.join().await;
    }

    #[tokio::test]
    async fn test_queue_all_agents_offline_plays_busy_prompt_that_is_playable() {
        if !packaged_sounds_available() {
            eprintln!("skipping: config/sounds/ not present");
            return;
        }
        // Sequential plan with multiple agents — every one will report busy,
        // mirroring the all-agents-offline / no-agents-available case.
        let mut config = build_queue_config_with_default_prompts();
        // Reuse the sequential agent list but keep default prompts/audio.
        let seq = build_sequential_queue_config();
        config.agents = seq.agents.clone();
        config.strategy = seq.strategy.clone();
        let plan = config.to_plan();

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        // discard hold music
        let _ = stack.next_cmd(200).await.expect("hold music");

        // Every agent is reported busy (offline-equivalent exhaustion path).
        stack.custom("all_agents_busy", serde_json::json!({}));

        let busy_cmd = stack
            .next_cmd(200)
            .await
            .expect("expected busy-prompt Play command");
        let path = play_path(&busy_cmd);
        assert!(
            path.ends_with("queue-busy-zh.wav"),
            "all-agents-unavailable should trigger the ZH busy prompt, got {path}"
        );
        assert_spec_is_playable("busy", &path).await;

        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_no_answer_agents_plays_no_answer_prompt_that_is_playable() {
        if !packaged_sounds_available() {
            eprintln!("skipping: config/sounds/ not present");
            return;
        }
        let mut config = build_queue_config_with_default_prompts();
        let seq = build_sequential_queue_config();
        config.agents = seq.agents.clone();
        config.strategy = seq.strategy.clone();
        let plan = config.to_plan();

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        let _ = stack.next_cmd(200).await.expect("hold music");

        // Sequential no-answer across every agent.
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_no_answer", serde_json::json!({}));

        let na_cmd = stack
            .next_cmd(200)
            .await
            .expect("expected no-answer-prompt Play command");
        let path = play_path(&na_cmd);
        assert!(
            path.ends_with("queue-no-answer-zh.wav"),
            "all-agents-no-answer should trigger the ZH no-answer prompt, got {path}"
        );
        assert_spec_is_playable("no-answer", &path).await;

        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }
}
