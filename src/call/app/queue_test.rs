//! Tests for the Queue application.
//!
//! Uses [`MockCallStack`] to drive a [`QueueApp`] through simulated events
//! without any SIP stack, media, or database.

#[cfg(test)]
mod tests {
    use crate::call::app::queue::{QueueApp, QueueConfig};
    use crate::call::app::testing::MockCallStack;
    use crate::call::app::CallApp;
    use crate::call::{DialStrategy, FailureAction, Location, QueueFallbackAction, QueueHoldConfig, QueuePlan};
    use crate::proxy::proxy_call::state::SessionAction;
    use rsip::Uri;
    use std::time::Duration;

    /// Build a minimal queue plan with a single agent for testing.
    fn build_simple_queue() -> QueuePlan {
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
        };

        QueuePlan {
            accept_immediately: true,
            passthrough_ringback: false,
            hold: Some(QueueHoldConfig {
                audio_file: Some("sounds/hold_music.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsip::StatusCode::TemporarilyUnavailable),
                reason: Some("All agents busy".to_string()),
            })),
            dial_strategy: Some(DialStrategy::Sequential(vec![location])),
            ring_timeout: Some(Duration::from_secs(30)),
            label: Some("test-queue".to_string()),
            retry_codes: None,
            no_trying_timeout: None,
        }
    }

    /// Build a queue plan with multiple agents for sequential dialing.
    fn build_sequential_queue() -> QueuePlan {
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
        })
        .collect();

        QueuePlan {
            accept_immediately: true,
            passthrough_ringback: false,
            hold: Some(QueueHoldConfig {
                audio_file: Some("sounds/hold_music.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsip::StatusCode::TemporarilyUnavailable),
                reason: Some("All agents busy".to_string()),
            })),
            dial_strategy: Some(DialStrategy::Sequential(agents)),
            ring_timeout: Some(Duration::from_secs(30)),
            label: Some("sequential-queue".to_string()),
            retry_codes: None,
            no_trying_timeout: None,
        }
    }

    /// Build a queue plan with parallel dialing.
    fn build_parallel_queue() -> QueuePlan {
        let agents: Vec<Location> = vec![
            "sip:agent1@example.com",
            "sip:agent2@example.com",
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
        })
        .collect();

        QueuePlan {
            accept_immediately: true,
            passthrough_ringback: false,
            hold: Some(QueueHoldConfig {
                audio_file: Some("sounds/hold_music.wav".to_string()),
                loop_playback: true,
            }),
            fallback: Some(QueueFallbackAction::Failure(FailureAction::Hangup {
                code: Some(rsip::StatusCode::TemporarilyUnavailable),
                reason: Some("All agents busy".to_string()),
            })),
            dial_strategy: Some(DialStrategy::Parallel(agents)),
            ring_timeout: Some(Duration::from_secs(30)),
            label: Some("parallel-queue".to_string()),
            retry_codes: None,
            no_trying_timeout: None,
        }
    }

    // ── 1. Basic queue enter with immediate answer and hold music ──

    #[tokio::test]
    async fn test_queue_basic_enter() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Queue should answer on enter (accept_immediately = true)
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Queue should start playing hold music
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/hold_music.wav")
            })
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 2. Queue without immediate answer ──

    #[tokio::test]
    async fn test_queue_no_immediate_answer() {
        let mut plan = build_simple_queue();
        plan.accept_immediately = false;

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Queue should NOT answer immediately
        // It should start hold music without answering
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/hold_music.wav")
            })
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 3. Queue with no agents - fallback to hangup ──

    #[tokio::test]
    async fn test_queue_no_agents_fallback() {
        let mut plan = build_simple_queue();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Queue should detect no agents and execute fallback immediately
        // No AcceptCall is sent because there are no agents to dial
        stack
            .assert_cmd(200, "Hangup", |c| {
                matches!(c, SessionAction::Hangup { reason: Some(crate::callrecord::CallRecordHangupReason::ServerUnavailable), code: Some(480), .. })
            })
            .await;
    }

    // ── 4. Queue fallback with play then hangup ──

    #[tokio::test]
    async fn test_queue_play_then_hangup_fallback() {
        let mut plan = build_simple_queue();
        plan.fallback = Some(QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
            audio_file: "sounds/all_busy.wav".to_string(),
            use_early_media: false,
            status_code: rsip::StatusCode::TemporarilyUnavailable,
            reason: Some("All agents are busy".to_string()),
        }));
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Queue detects no agents and executes fallback immediately
        // For PlayThenHangup, it currently just hangs up (play is skipped in current impl)
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 5. Queue hold music loops ──

    #[tokio::test]
    async fn test_queue_hold_music_completes() {
        let plan = build_simple_queue();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/hold_music.wav")
            })
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
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
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
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;

        // Simulate agent connected event
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent1@example.com"}),
        );

        // Should transfer to the connected agent
        stack
            .assert_cmd(200, "Transfer", |c| {
                matches!(c, SessionAction::TransferTarget(t) if t == "sip:agent1@example.com")
            })
            .await;

        stack.join().await.expect("should exit after transfer");
    }

    // ── 8. Queue with agent busy event - retry next agent ──

    #[tokio::test]
    async fn test_queue_agent_busy_retry() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;

        // First agent is busy
        stack.custom("agent_busy", serde_json::json!({}));

        // Should continue with next agent (no immediate action, continues waiting)
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Simulate second agent connected
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent2@example.com"}),
        );

        // Should transfer to the second agent
        stack
            .assert_cmd(200, "Transfer", |c| {
                matches!(c, SessionAction::TransferTarget(t) if t == "sip:agent2@example.com")
            })
            .await;

        stack.join().await.expect("should exit after transfer");
    }

    // ── 9. Queue with all agents busy - fallback ──

    #[tokio::test]
    async fn test_queue_all_agents_busy_fallback() {
        let plan = build_sequential_queue();
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Answer and start hold music
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;

        // All agents are busy
        stack.custom("all_agents_busy", serde_json::json!({}));

        // Should execute fallback (hangup)
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
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

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Queue detects no agents and executes redirect fallback
        stack
            .assert_cmd(200, "Transfer", |c| {
                matches!(c, SessionAction::TransferTarget(t) if t == "sip:backup@example.com")
            })
            .await;

        stack.join().await.expect("should exit after redirect");
    }

    // ── 11. Queue with queue-to-queue fallback ──

    #[tokio::test]
    async fn test_queue_to_queue_fallback() {
        let mut plan = build_simple_queue();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![]));
        plan.fallback = Some(QueueFallbackAction::Queue {
            name: "overflow".to_string(),
        });

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Queue detects no agents and executes queue transfer fallback
        stack
            .assert_cmd(200, "Transfer", |c| {
                matches!(c, SessionAction::TransferTarget(t) if t == "queue:overflow")
            })
            .await;

        stack.join().await.expect("should exit after queue transfer");
    }

    // ── 12. Queue with no hold music configured ──

    #[tokio::test]
    async fn test_queue_no_hold_music() {
        let mut plan = build_simple_queue();
        plan.hold = None;

        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
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
        let app = QueueApp::new(plan.clone());

        assert_eq!(app.name(), "test-queue");
        assert_eq!(app.app_type(), crate::call::app::CallAppType::Queue);
    }

    // ── 14. Queue app without label uses default ──

    #[tokio::test]
    async fn test_queue_app_name_default() {
        let mut plan = build_simple_queue();
        plan.label = None;
        let app = QueueApp::new(plan);

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
                code: Some(rsip::StatusCode::TemporarilyUnavailable),
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
            }],
            strategy: DialStrategy::Sequential(vec![]),
            ring_timeout: Some(Duration::from_secs(60)),
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
        let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan)), "caller", "1000");

        // Initial answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Hold music starts
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;

        // Agent 1 is busy
        stack.custom("agent_busy", serde_json::json!({}));

        // Agent 2 no answer
        stack.custom("agent_no_answer", serde_json::json!({}));

        // Agent 3 connects
        stack.custom(
            "agent_connected",
            serde_json::json!({"agent_uri": "sip:agent3@example.com"}),
        );

        // Should transfer to agent 3
        stack
            .assert_cmd(200, "Transfer", |c| {
                matches!(c, SessionAction::TransferTarget(t) if t == "sip:agent3@example.com")
            })
            .await;

        stack.join().await.expect("should complete successfully");
    }
}
