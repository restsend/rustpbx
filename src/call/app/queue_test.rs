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

    /// Build a registered-agent [`Location`] for the given SIP URI.
    fn test_location(uri: &str) -> Location {
        Location {
            aor: Uri::try_from(uri).unwrap(),
            expires: 3600,
            ..Default::default()
        }
    }

    /// Build a minimal queue config with a single agent for testing.
    fn build_simple_queue_config() -> QueueConfig {
        let location = test_location("sip:agent1@example.com");

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
        .map(test_location)
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
    fn build_parallel_queue_config() -> QueueConfig {
        let agents: Vec<Location> = vec!["sip:agent1@example.com", "sip:agent2@example.com"]
            .into_iter()
            .map(test_location)
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

        let hold_cmd = stack.next_cmd(200).await.expect("hold music Play");
        let hold_tid = play_track_id(&hold_cmd);

        // Hold music completed with a matching track id → restarted.
        stack.audio_complete(hold_tid);
        stack
            .assert_cmd(200, "PlayPrompt-restart", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;

        // A completion with a foreign track id is ignored (no restart).
        stack.audio_complete("foreign-track");
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
            agents: vec![test_location("sip:agent@example.com")],
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
        let busy_cmd = stack.next_cmd(200).await.expect("busy prompt Play");

        stack.audio_complete(play_track_id(&busy_cmd));

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
        let busy_cmd = stack.next_cmd(200).await.expect("busy prompt Play");

        stack.audio_complete(play_track_id(&busy_cmd));

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
        let config = config_with_service_prompt("sounds/queue-service-zh.wav");
        let plan = config.to_plan();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

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

        // Dialing starts → transfer prompt plays BEFORE any connection.
        stack.custom("dial_next_agent", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd", |c| matches!(c, CallCommand::LegAdd { .. }))
            .await;
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let transfer_cmd = stack.next_cmd(200).await.expect("transfer prompt Play");
        assert!(
            play_path(&transfer_cmd).ends_with("queue-transfer-zh.wav"),
            "expected the ZH transfer prompt"
        );
        assert!(
            play_is_side_only(&transfer_cmd),
            "transfer prompt must be caller-only"
        );
        let transfer_tid = play_track_id(&transfer_cmd);

        // Agent answers mid-prompt → prompt cut, caller-only service prompt next.
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "StopTransfer", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let service_cmd = stack.next_cmd(200).await.expect("service prompt Play");
        assert!(
            play_path(&service_cmd).ends_with("queue-service-zh.wav"),
            "expected the ZH service prompt"
        );
        assert!(
            play_is_side_only(&service_cmd),
            "service prompt must be caller-only"
        );
        let service_tid = play_track_id(&service_cmd);

        // Late natural completion of the cut transfer prompt must be ignored.
        stack.audio_complete(transfer_tid);

        stack.audio_complete(service_tid);

        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    #[tokio::test]
    async fn test_queue_transfer_prompt_completes_before_agent_answers() {
        let config = config_with_service_prompt("sounds/queue-service-zh.wav");
        let plan = config.to_plan();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

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

        stack.custom("dial_next_agent", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd", |c| matches!(c, CallCommand::LegAdd { .. }))
            .await;
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let transfer_cmd = stack.next_cmd(200).await.expect("transfer prompt Play");
        let transfer_tid = play_track_id(&transfer_cmd);

        // Prompt finishes while the agent is still ringing → hold music resumes.
        stack.audio_complete(transfer_tid);
        let hold_cmd = stack.next_cmd(200).await.expect("hold music resume Play");
        assert!(
            play_path(&hold_cmd).ends_with("hold_music.wav"),
            "hold music must resume after the transfer prompt"
        );

        // Agent answers later → connect with the service prompt.
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "StopHold2", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let service_cmd = stack.next_cmd(200).await.expect("service prompt Play");
        let service_tid = play_track_id(&service_cmd);
        stack.audio_complete(service_tid);

        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    #[tokio::test]
    async fn test_no_service_prompt_connects_directly() {
        // Default prompts without service_prompt: connect right after cutting
        // the transfer prompt, no post-connect announcement.
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

        stack.custom("dial_next_agent", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd", |c| matches!(c, CallCommand::LegAdd { .. }))
            .await;
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

        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "StopTransfer", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;

        stack
            .join()
            .await
            .expect("should exit immediately without a service prompt");
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

        let busy_cmd = stack.next_cmd(200).await.expect("busy prompt Play");
        stack.audio_complete(play_track_id(&busy_cmd));

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
        // First originate starts the pre-connect transfer prompt.
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let transfer_cmd = stack.next_cmd(200).await.expect("transfer prompt Play");
        assert!(play_is_side_only(&transfer_cmd));

        stack.custom("agent_busy", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        // Retry must NOT replay the transfer prompt.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "transfer prompt must play only once per queue entry"
        );

        stack.custom("agent_busy", serde_json::json!({}));

        let busy_cmd = stack.next_cmd(200).await.expect("busy prompt Play");
        stack.audio_complete(play_track_id(&busy_cmd));

        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_queue_transfer_prompt_english() {
        let mut config = build_simple_queue_config();
        config.voice_prompts = Some(VoicePrompts {
            service_prompt: Some("sounds/queue-service-en.wav".to_string()),
            ..VoicePrompts::en()
        });

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

        // Dialing starts → EN transfer prompt before connect.
        stack.custom("dial_next_agent", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd", |c| matches!(c, CallCommand::LegAdd { .. }))
            .await;
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let transfer_cmd = stack.next_cmd(200).await.expect("transfer prompt Play");
        assert!(
            play_path(&transfer_cmd).ends_with("queue-transfer-en.wav"),
            "expected the EN transfer prompt"
        );

        // Agent answers → EN service prompt after connect.
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "StopTransfer", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let service_cmd = stack.next_cmd(200).await.expect("service prompt Play");
        assert!(
            play_path(&service_cmd).ends_with("queue-service-en.wav"),
            "expected the EN service prompt"
        );

        stack.audio_complete(play_track_id(&service_cmd));

        stack
            .join()
            .await
            .expect("should exit after english service prompt");
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

        let busy_cmd = stack.next_cmd(200).await.expect("busy prompt Play");
        stack.audio_complete(play_track_id(&busy_cmd));

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
        // First originate starts the pre-connect transfer prompt.
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
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_no_answer", serde_json::json!({}));

        // Should play no-answer prompt (not busy prompt)
        let na_cmd = stack.next_cmd(200).await.expect("no-answer prompt Play");
        stack.audio_complete(play_track_id(&na_cmd));

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
        // First originate starts the pre-connect transfer prompt.
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
        stack.custom("agent_no_answer", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-agent3", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.custom("agent_busy", serde_json::json!({}));

        // Last one was busy, so should play busy prompt
        let busy_cmd = stack.next_cmd(200).await.expect("busy prompt Play");
        stack.audio_complete(play_track_id(&busy_cmd));

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

        let na_cmd = stack.next_cmd(200).await.expect("no-answer prompt Play");
        stack.audio_complete(play_track_id(&na_cmd));

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

    // ── Final Destination Prompt ──

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
        let final_cmd = stack.next_cmd(200).await.expect("final prompt Play");

        // Final prompt audio completes → fallback (hangup)
        stack.audio_complete(play_track_id(&final_cmd));
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;

        stack.join().await.unwrap();
    }

    // ── Escalation ──

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
            fair: false,
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

    // ── Escalation: auto-armed timer + fair union widening ──────────────

    /// AgentRegistry double that delegates presence duties to
    /// [`MemoryRegistry`] and records the escalation + skill-group lifecycle
    /// hook invocations for assertions.
    struct HookRecordingRegistry {
        inner: crate::call::app::agent_registry::memory::MemoryRegistry,
        escalation_calls: std::sync::Mutex<Vec<(String, Vec<String>, bool)>>,
        /// URIs returned by resolve_escalation_targets (rotated per call so
        /// repeated invocations can be distinguished).
        escalation_uris: Vec<Vec<String>>,
        /// URIs returned by `resolve_target` / `resolve_target_with_policy`
        /// (rotated per call). When empty, falls through to `inner`.
        resolve_uris: Vec<Vec<String>>,
        resolve_calls: std::sync::Mutex<usize>,
        abandoned: std::sync::Mutex<Vec<(String, String, u64)>>,
        timeouts: std::sync::Mutex<Vec<(String, String, u64)>>,
        fallbacks: std::sync::Mutex<Vec<(String, String, String, String)>>,
    }

    impl HookRecordingRegistry {
        fn new() -> Self {
            Self {
                inner: crate::call::app::agent_registry::memory::MemoryRegistry::new(),
                escalation_calls: std::sync::Mutex::new(Vec::new()),
                escalation_uris: Vec::new(),
                resolve_uris: Vec::new(),
                resolve_calls: std::sync::Mutex::new(0),
                abandoned: std::sync::Mutex::new(Vec::new()),
                timeouts: std::sync::Mutex::new(Vec::new()),
                fallbacks: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_escalation_uris(mut self, uris: Vec<Vec<String>>) -> Self {
            self.escalation_uris = uris;
            self
        }

        fn with_resolve_uris(mut self, uris: Vec<Vec<String>>) -> Self {
            self.resolve_uris = uris;
            self
        }

        fn next_resolve_uris(&self) -> Option<Vec<String>> {
            if self.resolve_uris.is_empty() {
                return None;
            }
            let mut n = self.resolve_calls.lock().unwrap();
            let idx = (*n).min(self.resolve_uris.len() - 1);
            *n += 1;
            Some(self.resolve_uris[idx].clone())
        }
    }

    #[async_trait::async_trait]
    impl AgentRegistry for HookRecordingRegistry {
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
            if let Some(uris) = self.next_resolve_uris() {
                return uris;
            }
            self.inner.resolve_target(target_uri).await
        }

        async fn resolve_target_with_policy(
            &self,
            target_uri: &str,
            _policy: Option<&str>,
            _call_id: &str,
        ) -> Vec<String> {
            if let Some(uris) = self.next_resolve_uris() {
                return uris;
            }
            self.inner
                .resolve_target_with_policy(target_uri, _policy, _call_id)
                .await
        }

        async fn resolve_escalation_targets(
            &self,
            primary_target_uri: &str,
            add_group_ids: &[String],
            _call_id: &str,
            fair: bool,
        ) -> Vec<String> {
            self.escalation_calls.lock().unwrap().push((
                primary_target_uri.to_string(),
                add_group_ids.to_vec(),
                fair,
            ));
            let calls = self.escalation_calls.lock().unwrap().len();
            self.escalation_uris
                .get(calls - 1)
                .cloned()
                .unwrap_or_default()
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

    /// The escalation timer must arm itself in `on_enter` and fire WITHOUT
    /// any manual `stack.timeout("escalation_check")` — the dormant-path
    /// regression (timer only ever re-armed after firing) is exactly what
    /// this test forbids.
    #[tokio::test]
    async fn test_escalation_timer_auto_arms_and_widens_fairly() {
        use std::sync::Arc;

        let mut config = build_simple_queue_config();
        config.escalation_mode = crate::call::app::queue::EscalationMode::Cumulative;
        config.escalation_timeline = vec![crate::call::app::queue::EscalationStep {
            threshold_secs: 1,
            add_skill_group: "support_l2".to_string(),
            fair: true,
        }];
        config.skill_group = Some("support".to_string());

        // Union resolve returns the already-dialled primary FIRST plus one
        // widened agent — cumulative escalation must skip the duplicate
        // primary leg and dial only the new agent.
        let registry = Arc::new(HookRecordingRegistry::new().with_escalation_uris(vec![vec![
            "sip:agent1@example.com".to_string(), // primary, already ringing
            "sip:l2agent@example.com".to_string(), // widened fair pick
        ]]));

        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config)
            .with_agent_registry(registry.clone())
            .with_call_id("call-esc-1".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        // on_enter: answer + hold music + dial the primary agent (no manual
        // timer fire). Sequential dialing is kicked off by the production
        // execute_flow's "dial_next_agent" injection.
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "Hold", |c| matches!(c, CallCommand::Play { .. }))
            .await;
        stack.custom("dial_next_agent", serde_json::json!({}));
        stack.assert_cmd(200, "LegAdd-primary", |c| {
            matches!(c, CallCommand::LegAdd { target, .. } if target.contains("agent1@example.com"))
        })
        .await;

        // The auto-armed escalation timer fires by itself after the 1s
        // threshold; the widened union resolves and dials ONLY the new agent
        // (the duplicate primary leg is filtered out).
        stack.assert_cmd(4000, "LegAdd-widened", |c| {
            matches!(c, CallCommand::LegAdd { target, .. } if target.contains("l2agent@example.com"))
        })
        .await;

        // No further LegAdd beyond the two above within a quiet window.
        assert!(
            stack.next_cmd(1200).await.is_none(),
            "no duplicate primary leg or spurious dial after escalation"
        );

        // The addon saw the fair union resolve with the primary group + step.
        let calls = registry.escalation_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "exactly one escalation resolve");
        assert_eq!(calls[0].0, "skill-group:support");
        assert_eq!(calls[0].1, vec!["support_l2".to_string()]);
        assert!(calls[0].2, "fair flag must reach the registry");

        stack.cancel();
        let _ = stack.join().await;
    }

    /// Once every timeline step has triggered the escalation timer stops
    /// re-arming — no idle wake-ups for the rest of the call.
    #[tokio::test]
    async fn test_escalation_timer_stops_after_all_steps() {
        use std::sync::Arc;

        let mut config = build_simple_queue_config();
        config.escalation_mode = crate::call::app::queue::EscalationMode::Cumulative;
        config.escalation_timeline = vec![crate::call::app::queue::EscalationStep {
            threshold_secs: 1,
            add_skill_group: "support_l2".to_string(),
            fair: true,
        }];
        config.skill_group = Some("support".to_string());

        let registry = Arc::new(
            HookRecordingRegistry::new()
                .with_escalation_uris(vec![vec!["sip:l2agent@example.com".to_string()]]),
        );

        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config)
            .with_agent_registry(registry.clone())
            .with_call_id("call-esc-2".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "Hold", |c| matches!(c, CallCommand::Play { .. }))
            .await;
        stack.custom("dial_next_agent", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd-primary", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack.assert_cmd(4000, "LegAdd-widened", |c| {
            matches!(c, CallCommand::LegAdd { target, .. } if target.contains("l2agent@example.com"))
        })
        .await;

        // The historical behavior re-armed a 10s wake-up forever even after
        // the last step; give the timer ample time to misfire.
        assert!(
            stack.next_cmd(2000).await.is_none(),
            "escalation must not re-check after the last step triggered"
        );
        assert_eq!(
            registry.escalation_calls.lock().unwrap().len(),
            1,
            "no additional escalation resolves after the timeline is done"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    /// Replace-mode escalation over a skill-group union: when the widened
    /// resolve returns the already-dialled primary agent plus a new agent,
    /// Replace must (1) tear down the pending primary leg via LegRemove,
    /// (2) skip the duplicate primary, and (3) dial ONLY the new agent.
    #[tokio::test]
    async fn test_escalation_replace_mode_removes_pending_leg_and_dials_union() {
        use std::sync::Arc;

        let mut config = build_simple_queue_config();
        config.escalation_mode = crate::call::app::queue::EscalationMode::Replace;
        config.escalation_timeline = vec![crate::call::app::queue::EscalationStep {
            threshold_secs: 1,
            add_skill_group: "support_l2".to_string(),
            fair: true,
        }];
        config.skill_group = Some("support".to_string());

        let registry = Arc::new(HookRecordingRegistry::new().with_escalation_uris(vec![vec![
            "sip:agent1@example.com".to_string(), // primary, already ringing
            "sip:l2agent@example.com".to_string(), // widened fair pick
        ]]));

        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config)
            .with_agent_registry(registry.clone())
            .with_call_id("call-esc-replace".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "Hold", |c| matches!(c, CallCommand::Play { .. }))
            .await;
        stack.custom("dial_next_agent", serde_json::json!({}));
        stack.assert_cmd(200, "LegAdd-primary", |c| {
            matches!(c, CallCommand::LegAdd { target, .. } if target.contains("agent1@example.com"))
        })
        .await;

        // Replace escalation: the pending primary leg is removed first …
        stack
            .assert_cmd(4000, "LegRemove-primary", |c| {
                matches!(c, CallCommand::LegRemove { .. })
            })
            .await;
        // … then ONLY the widened agent is dialled (no duplicate primary).
        stack.assert_cmd(2000, "LegAdd-widened", |c| {
            matches!(c, CallCommand::LegAdd { target, .. } if target.contains("l2agent@example.com"))
        })
        .await;
        assert!(
            stack.next_cmd(1200).await.is_none(),
            "replace escalation must not re-dial the removed primary"
        );
        assert_eq!(
            registry.escalation_calls.lock().unwrap().len(),
            1,
            "exactly one union resolve for the replace step"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    /// `next_escalation_check_delay` clamps long thresholds to the 10s
    /// polling cadence, floors short ones at 1s, and stops (None) once every
    /// step has triggered.
    #[tokio::test]
    async fn test_next_escalation_check_delay_clamps_and_stops() {
        let mut config = build_simple_queue_config();
        config.escalation_timeline = vec![
            crate::call::app::queue::EscalationStep {
                threshold_secs: 1,
                add_skill_group: "l1".to_string(),
                fair: false,
            },
            crate::call::app::queue::EscalationStep {
                threshold_secs: 3600,
                add_skill_group: "far_away".to_string(),
                fair: false,
            },
        ];

        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config.clone());
        // Freshly enqueued: the earliest un-triggered threshold is 1s away.
        assert_eq!(
            queue.next_escalation_check_delay(),
            Some(Duration::from_secs(1)),
            "1s threshold must fire at the 1s floor"
        );

        // Only a far step left: degrade to the 10s polling cadence.
        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config.clone());
        queue.escalated_groups = vec!["l1".to_string()];
        assert_eq!(
            queue.next_escalation_check_delay(),
            Some(Duration::from_secs(10)),
            "3600s threshold must clamp to 10s"
        );

        // Every step triggered: the timer stops re-arming.
        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue.escalated_groups = vec!["l1".to_string(), "far_away".to_string()];
        assert_eq!(
            queue.next_escalation_check_delay(),
            None,
            "no wake-up after every step has triggered"
        );
    }

    /// `agent_availability`: unknown agent → None (legacy dial), own-call
    /// Ringing reservation → available, other-call Ringing → unavailable.
    #[tokio::test]
    async fn test_agent_availability_reservation_semantics() {
        use crate::call::app::agent_registry::{AgentRegistry, PresenceState};

        let registry = HookRecordingRegistry::new();
        registry
            .register(
                "agent1".to_string(),
                "Agent One".to_string(),
                "sip:agent1@example.com".to_string(),
                vec!["support".to_string()],
                1,
            )
            .await
            .unwrap();

        // Idle agent with capacity: available.
        registry
            .update_presence("agent1", PresenceState::Idle)
            .await
            .unwrap();
        assert_eq!(
            QueueApp::agent_availability(&registry, "sip:agent1@example.com", "call-a").await,
            Some(true)
        );

        // Reserved for OUR call (Idle → Ringing{call-a}): still dialable —
        // the double-dial fix must not strand the call that reserved it.
        registry
            .update_presence(
                "agent1",
                PresenceState::Ringing {
                    call_id: Some("call-a".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            QueueApp::agent_availability(&registry, "sip:agent1@example.com", "call-a").await,
            Some(true),
            "Ringing reserved for the same call must stay available"
        );
        assert_eq!(
            QueueApp::agent_availability(&registry, "sip:agent1@example.com", "call-b").await,
            Some(false),
            "Ringing reserved for another call must be unavailable"
        );

        // Busy on another call: unavailable.
        registry
            .update_presence(
                "agent1",
                PresenceState::Busy {
                    call_id: Some("call-b".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            QueueApp::agent_availability(&registry, "sip:agent1@example.com", "call-a").await,
            Some(false)
        );

        // Unknown agent (not in registry, not resolvable by user part): the
        // caller keeps the legacy dial behavior.
        assert_eq!(
            QueueApp::agent_availability(&registry, "sip:ghost@example.com", "call-a").await,
            None
        );
        // Empty registry path: also None (no false negatives).
        let empty = HookRecordingRegistry::new();
        assert_eq!(
            QueueApp::agent_availability(&empty, "sip:agent1@example.com", "call-a").await,
            None
        );
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

    fn play_url(cmd: &CallCommand) -> String {
        match cmd {
            CallCommand::Play {
                source: MediaSource::Url { url },
                ..
            } => url.clone(),
            other => panic!("expected CallCommand::Play with Url source, got {other:?}"),
        }
    }

    fn play_track_id(cmd: &CallCommand) -> String {
        match cmd {
            CallCommand::Play { options, .. } => options
                .as_ref()
                .and_then(|o| o.track_id.clone())
                .expect("Play command without track_id"),
            other => panic!("expected CallCommand::Play, got {other:?}"),
        }
    }

    fn play_is_side_only(cmd: &CallCommand) -> bool {
        match cmd {
            CallCommand::Play { options, .. } => options.as_ref().is_some_and(|o| o.side_only),
            other => panic!("expected CallCommand::Play, got {other:?}"),
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

    /// A skill-routed queue with no agents available must notify the dispatcher
    /// of the abandoned call and the executed fallback.
    #[tokio::test]
    async fn test_queue_skill_abandon_notifies_registry() {
        use std::sync::Arc;
        let registry = Arc::new(HookRecordingRegistry::new());
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
        let registry = Arc::new(HookRecordingRegistry::new());
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

    /// Skill-group with no Idle agents enters wait retention (hold), not
    /// immediate busy fallback; hangup notifies abandoned with skill_group id.
    #[tokio::test]
    async fn test_queue_wait_retention_abandon_notifies() {
        use std::sync::Arc;
        let registry = Arc::new(
            HookRecordingRegistry::new().with_resolve_uris(vec![vec![], vec![]]),
        );
        let mut config = build_simple_queue_config();
        config.skill_routing_enabled = true;
        config.skill_group = Some("support".to_string());
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.hold = Some(QueueHoldConfig {
            audio_file: Some("sounds/hold_music.wav".to_string()),
            loop_playback: true,
        });
        config.retry_interval_secs = 5;
        config.max_wait_secs = 300;
        config.voice_prompts = Some(crate::call::VoicePrompts {
            busy_prompt: Some("sounds/queue-busy-zh.wav".to_string()),
            comfort_prompts: vec![crate::call::ComfortPrompt {
                audio_file: "sounds/queue-busy-zh.wav".to_string(),
                interval_secs: 30,
            }],
            ..Default::default()
        });

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(registry.clone());
        queue = queue.with_call_id("call-wait-1".to_string());
        queue = queue.with_skill_group("support".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        let hold_cmd = stack
            .next_cmd(200)
            .await
            .expect("expected hold music while waiting");
        assert!(
            matches!(hold_cmd, CallCommand::Play { .. }),
            "wait retention must play hold/comfort, got {hold_cmd:?}"
        );
        stack.cancel();
        let _ = stack.join().await;

        let abandoned = registry.abandoned.lock().unwrap();
        assert!(
            abandoned
                .iter()
                .any(|(c, q, _)| c == "call-wait-1" && q == "support"),
            "wait-retention hangup must notify abandoned for skill group, got {abandoned:?}"
        );
    }

    /// Wait retention poll (`queue_retry`) dials when resolve returns an Idle agent.
    #[tokio::test]
    async fn test_queue_wait_retention_retry_dials_when_idle() {
        use std::sync::Arc;
        let registry = Arc::new(HookRecordingRegistry::new().with_resolve_uris(vec![
            vec![],
            vec!["sip:agent-1@localhost".to_string()],
        ]));
        let mut config = build_simple_queue_config();
        config.skill_routing_enabled = true;
        config.skill_group = Some("support".to_string());
        config.agents = vec![];
        config.strategy = DialStrategy::Sequential(vec![]);
        config.hold = Some(QueueHoldConfig {
            audio_file: Some("sounds/hold_music.wav".to_string()),
            loop_playback: true,
        });
        config.retry_interval_secs = 1;
        config.max_wait_secs = 300;
        config.accept_immediately = true;

        let plan = config.to_plan();
        let mut queue = QueueApp::new(plan, config);
        queue = queue.with_agent_registry(registry.clone());
        queue = queue.with_call_id("call-wait-2".to_string());
        queue = queue.with_skill_group("support".to_string());

        let mut stack = MockCallStack::run(Box::new(queue), "1001", "1002");
        stack
            .assert_cmd(200, "Answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        let _hold = stack.next_cmd(200).await.expect("hold while waiting");

        stack.timeout("queue_retry");

        let mut saw_originate = false;
        for _ in 0..5 {
            if let Some(cmd) = stack.next_cmd(300).await {
                if matches!(cmd, CallCommand::LegAdd { .. }) {
                    saw_originate = true;
                    break;
                }
            }
        }
        assert!(
            saw_originate,
            "queue_retry with Idle agent must originate a dial"
        );
        stack.cancel();
        let _ = stack.join().await;
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

        let registry = Arc::new(HookRecordingRegistry::new());
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
        let (_, mut config) = build_plan_and_config_with_default_audio();
        config.voice_prompts = Some(VoicePrompts {
            service_prompt: Some(crate::call::DEFAULT_QUEUE_SERVICE_PROMPT_ZH.to_string()),
            ..VoicePrompts::zh()
        });
        let plan = config.to_plan();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        // discard hold music
        let _ = stack.next_cmd(200).await.expect("hold music");

        // Dialing starts → pre-connect transfer prompt.
        stack.custom("dial_next_agent", serde_json::json!({}));
        stack
            .assert_cmd(200, "LegAdd", |c| matches!(c, CallCommand::LegAdd { .. }))
            .await;
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
            "dialing an online agent should trigger the ZH transfer prompt, got {path}"
        );
        assert_spec_is_playable("transfer", &path).await;
        assert!(play_is_side_only(&transfer_cmd));

        // Agent answers → caller-only service prompt after connect.
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "StopTransfer", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let service_cmd = stack
            .next_cmd(200)
            .await
            .expect("expected service-prompt Play command");
        let path = play_path(&service_cmd);
        assert!(
            path.ends_with("queue-service-zh.wav"),
            "connect should trigger the ZH service prompt, got {path}"
        );
        assert_spec_is_playable("service", &path).await;
        assert!(play_is_side_only(&service_cmd));

        stack.audio_complete(play_track_id(&service_cmd));
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

        stack.audio_complete(play_track_id(&busy_cmd));
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
        // First originate starts the pre-connect transfer prompt.
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

        stack.audio_complete(play_track_id(&na_cmd));
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    fn config_with_service_prompt(service_prompt: &str) -> QueueConfig {
        let mut config = build_simple_queue_config();
        config.voice_prompts = Some(VoicePrompts {
            service_prompt: Some(service_prompt.to_string()),
            ..VoicePrompts::zh()
        });
        config
    }

    async fn drive_to_service_prompt(stack: &mut MockCallStack) -> CallCommand {
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .next_cmd(200)
            .await
            .expect("expected service-prompt Play command")
    }

    #[tokio::test]
    async fn test_service_prompt_agent_name_from_registry() {
        use crate::call::app::agent_registry::memory::MemoryRegistry;
        use std::sync::Arc;

        let registry = Arc::new(MemoryRegistry::new());
        registry
            .register(
                "agent-001".into(),
                "小张".into(),
                "sip:agent1@example.com".into(),
                vec![],
                1,
            )
            .await
            .unwrap();

        let config = config_with_service_prompt("sounds/agents/{agent}-service.wav");
        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config).with_agent_registry(registry);
        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        let service_cmd = drive_to_service_prompt(&mut stack).await;
        assert_eq!(play_path(&service_cmd), "sounds/agents/小张-service.wav");
        assert!(play_is_side_only(&service_cmd));

        stack.audio_complete(play_track_id(&service_cmd));
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    #[tokio::test]
    async fn test_service_prompt_url_template_percent_encodes_name() {
        use crate::call::app::agent_registry::memory::MemoryRegistry;
        use std::sync::Arc;

        let registry = Arc::new(MemoryRegistry::new());
        registry
            .register(
                "agent-001".into(),
                "张 三".into(),
                "sip:agent1@example.com".into(),
                vec![],
                1,
            )
            .await
            .unwrap();

        let config = config_with_service_prompt("http://tts.local/say?text={agent}为您服务");
        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config).with_agent_registry(registry);
        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        let service_cmd = drive_to_service_prompt(&mut stack).await;
        let url = play_url(&service_cmd);
        assert!(
            url.starts_with("http://tts.local/say?text="),
            "URL template must be kept intact, got {url}"
        );
        assert!(
            url.contains("%E5%BC%A0%20%E4%B8%89"),
            "agent name must be percent-encoded inside URLs, got {url}"
        );

        stack.audio_complete(play_track_id(&service_cmd));
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    #[tokio::test]
    async fn test_service_prompt_without_registry_uses_uri_user() {
        let config = config_with_service_prompt("sounds/agents/{agent}-service.wav");
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

        let service_cmd = drive_to_service_prompt(&mut stack).await;
        assert_eq!(play_path(&service_cmd), "sounds/agents/agent1-service.wav");

        stack.audio_complete(play_track_id(&service_cmd));
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    #[tokio::test]
    async fn test_interrupted_prompt_event_after_connect_is_ignored() {
        let config = config_with_service_prompt("sounds/svc.wav");
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

        let service_cmd = drive_to_service_prompt(&mut stack).await;
        let service_tid = play_track_id(&service_cmd);

        // Bridge-cut interruption of the (never started) transfer prompt and
        // foreign completions must not disturb the service-prompt state.
        stack.audio_interrupted("transfer-track");
        stack.audio_complete("foreign-track");
        tokio::time::sleep(Duration::from_millis(50)).await;

        stack.audio_complete(service_tid);
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    #[tokio::test]
    async fn test_parallel_dial_plays_transfer_prompt_once() {
        let mut config = build_parallel_queue_config();
        config.voice_prompts = Some(VoicePrompts {
            service_prompt: Some("sounds/queue-service-zh.wav".to_string()),
            ..VoicePrompts::zh()
        });
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

        stack
            .assert_cmd(200, "LegAdd-agent1", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;
        stack
            .assert_cmd(200, "LegAdd-agent2", |c| {
                matches!(c, CallCommand::LegAdd { .. })
            })
            .await;

        stack
            .assert_cmd(200, "StopHold", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let transfer_cmd = stack.next_cmd(200).await.expect("transfer prompt Play");
        assert!(play_is_side_only(&transfer_cmd));

        // No further prompt commands while both agents ring.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "transfer prompt must play once"
        );

        // First answer connects; the other leg is cancelled, prompt cut.
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );
        stack
            .assert_cmd(200, "LegRemove-agent2", |c| {
                matches!(c, CallCommand::LegRemove { .. })
            })
            .await;
        stack
            .assert_cmd(200, "StopTransfer", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        let service_cmd = stack.next_cmd(200).await.expect("service prompt Play");
        assert!(play_is_side_only(&service_cmd));

        stack.audio_complete(play_track_id(&service_cmd));
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    // ── Regression: a late ring timeout after the agent answered must not
    // dial the next fallback agent into the already-bridged call. ──

    #[tokio::test]
    async fn test_ring_timeout_after_connect_does_not_dial_next_agent() {
        let config = config_with_service_prompt("sounds/queue-service-zh.wav");
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

        // Agent answers; queue plays the caller-only service prompt and
        // stays alive in PlayingServicePrompt (the window where the stale
        // ring timer used to fire).
        let service_cmd = drive_to_service_prompt(&mut stack).await;
        let service_tid = play_track_id(&service_cmd);

        // Stale ring-timeout fire must be ignored: no new LegAdd for the
        // next fallback agent, no Hangup, no fallback prompt.
        stack.timeout("agent_ring_timeout");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "stale ring timeout after connect must not produce any command"
        );

        // The connected call still completes normally.
        stack.audio_complete(service_tid);
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    // ── Regression: stale leg-failure events (e.g. parallel legs cancelled
    // by agent_connected) must not touch the connected call. ──

    #[tokio::test]
    async fn test_agent_failure_events_after_connect_are_ignored() {
        let config = config_with_service_prompt("sounds/queue-service-zh.wav");
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

        let service_cmd = drive_to_service_prompt(&mut stack).await;
        let service_tid = play_track_id(&service_cmd);

        // Stale leg failures after connect must be ignored entirely.
        stack.custom(
            "agent_busy",
            serde_json::json!({"agent_id": "agent-001", "leg_id": "leg-x"}),
        );
        stack.custom(
            "agent_no_answer",
            serde_json::json!({"agent_id": "agent-001", "leg_id": "leg-y"}),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "stale agent failure events after connect must not produce any command"
        );

        stack.audio_complete(service_tid);
        stack
            .join()
            .await
            .expect("should exit after service prompt");
    }

    // ── Regression: sequential fallback must skip agents that became
    // unavailable (busy/reserved by another concurrent call) before dialing.──

    #[tokio::test]
    async fn test_sequential_fallback_skips_unavailable_agent() {
        use crate::call::app::agent_registry::PresenceState;
        use crate::call::app::agent_registry::memory::MemoryRegistry;
        use std::sync::Arc;

        let registry = Arc::new(MemoryRegistry::new());
        for (id, uri) in [
            ("agent-001", "sip:agent1@example.com"),
            ("agent-002", "sip:agent2@example.com"),
            ("agent-003", "sip:agent3@example.com"),
        ] {
            registry
                .register(id.to_string(), id.to_string(), uri.to_string(), vec![], 1)
                .await
                .unwrap();
        }
        // agent-002 is busy on another call; the fallback must skip it.
        registry
            .update_presence(
                "agent-002",
                PresenceState::Busy {
                    call_id: Some("other-call".to_string()),
                },
            )
            .await
            .unwrap();

        let config = build_sequential_queue_config();
        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config).with_agent_registry(registry);
        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Kick off sequential dialing: agent1 is available and gets dialed.
        stack.custom("dial_next_agent", serde_json::json!({}));
        let first = stack.next_cmd(200).await.expect("LegAdd agent1");
        match &first {
            CallCommand::LegAdd { target, .. } => {
                assert_eq!(target, "sip:agent1@example.com")
            }
            other => panic!("expected LegAdd, got {other:?}"),
        }

        // Agent1 does not answer: the ring timeout advances the fallback,
        // which must skip the busy agent2 and dial agent3 directly.
        stack.timeout("agent_ring_timeout");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cmds = stack.drain_cmds();
        let dialed: Vec<&String> = cmds
            .iter()
            .filter_map(|c| match c {
                CallCommand::LegAdd { target, .. } => Some(target),
                _ => None,
            })
            .collect();
        assert_eq!(
            dialed,
            ["sip:agent3@example.com"],
            "fallback must skip busy agent2 and dial agent3 (all commands: {cmds:?})"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, CallCommand::Hangup(_))),
            "agent2 busy must not end the queue attempt"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── Regression: parallel dialing must not INVITE agents that are
    // already busy on other calls. ──

    #[tokio::test]
    async fn test_parallel_dial_skips_unavailable_agents() {
        use crate::call::app::agent_registry::PresenceState;
        use crate::call::app::agent_registry::memory::MemoryRegistry;
        use std::sync::Arc;

        let registry = Arc::new(MemoryRegistry::new());
        for (id, uri) in [
            ("agent-001", "sip:agent1@example.com"),
            ("agent-002", "sip:agent2@example.com"),
        ] {
            registry
                .register(id.to_string(), id.to_string(), uri.to_string(), vec![], 1)
                .await
                .unwrap();
        }
        // agent-002 is busy on another call; parallel dial must skip it.
        registry
            .update_presence(
                "agent-002",
                PresenceState::Busy {
                    call_id: Some("other-call".to_string()),
                },
            )
            .await
            .unwrap();

        let config = build_parallel_queue_config();
        let plan = config.to_plan();
        let queue = QueueApp::new(plan, config).with_agent_registry(registry);
        let mut stack = MockCallStack::run(Box::new(queue), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Only the available agent is dialed.
        let cmd = stack.next_cmd(200).await.expect("LegAdd agent1");
        match &cmd {
            CallCommand::LegAdd { target, .. } => {
                assert_eq!(target, "sip:agent1@example.com")
            }
            other => panic!("expected LegAdd, got {other:?}"),
        }
        assert!(
            stack.next_cmd(150).await.is_none(),
            "busy agent2 must not be dialed in parallel mode"
        );

        stack.cancel();
        let _ = stack.join().await;
    }
}
