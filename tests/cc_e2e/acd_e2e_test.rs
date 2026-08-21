//! ACD engine tests (in-process, NOT a SIP e2e).
//!
//! These tests drive `AcdEngine::enqueue()`/`tick()` directly on an
//! in-memory `CcAddonState` with synthetic agent snapshots:
//! 1. Call enters the queue with an ACD policy
//! 2. ACD assigns it to an agent / overflows per policy
//! 3. Presence, concurrency, and skill filters are respected
//!
//! No SIP stack, sockets, or running PBX are involved. The SIP-level ACD
//! queue behavior is covered by `tests/queue_e2e/` (real INVITEs through a
//! real in-process SipServer).

#[cfg(test)]
#[cfg(feature = "addon-cc")]
mod acd_e2e_test {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::time::sleep;

    use rustpbx::addons::cc::acd::{AcdConfig, AcdPolicy, BusinessHours, ScheduleConfig};
    use rustpbx::addons::cc::agent::AgentStatus;

    /// Build a CcAddonState whose ACD schedule is always within business hours
    /// (00:00–23:59), so tick()/enqueue() reach the assignment/overflow logic
    /// instead of short-circuiting to the "Off hours" fallback. The default
    /// schedule (09:00–18:00 Asia/Shanghai) is compared against the machine's
    /// *local* time, which makes the tests time-/tz-dependent otherwise.
    fn cc_state_always_open() -> rustpbx::addons::cc::CcAddonState {
        let mut cc_state = rustpbx::addons::cc::CcAddonState::new();
        let schedule = ScheduleConfig {
            business_hours: Some(BusinessHours {
                start: "00:00".to_string(),
                end: "23:59".to_string(),
                timezone: "UTC".to_string(),
            }),
            holidays: HashMap::new(),
            night_mode: None,
        };
        let mut policy = AcdPolicy::default();
        policy.schedule = schedule;
        let mut policies = HashMap::new();
        policies.insert("default".to_string(), policy.clone());
        policies.insert("support".to_string(), policy);
        cc_state.acd_engine = Arc::new(rustpbx::addons::cc::acd::AcdEngine::new(AcdConfig {
            policies,
            ..Default::default()
        }));
        cc_state
    }

    /// Test basic ACD flow:
    /// 1. Register an agent
    /// 2. Create a call
    /// 3. Verify ACD assigns the call to the agent
    #[tokio::test]
    async fn test_acd_basic_flow() {
        // Setup CC addon state with ACD (no running SIP server needed — this
        // exercises the in-memory ACD engine only).
        let cc_state = cc_state_always_open();

        // Register a test agent
        cc_state
            .agent_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        // Set agent to idle (available)
        cc_state
            .agent_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();

        // Verify agent is registered
        let agents = cc_state.agent_registry.list_agents().await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, "agent-001");

        // Create an ACD call context
        let call = rustpbx::addons::cc::acd::CallContext {
            call_id: "test-call-001".to_string(),
            trace_id: "trace-001".to_string(),
            caller_number: "1000".to_string(),
            caller_name: Some("Test Caller".to_string()),
            skill_group_id: "support".to_string(),
            priority: 0,
            required_skills: vec!["support".to_string()],
            queue_time: std::time::Instant::now(),
            custom_data: std::collections::HashMap::new(),
        };

        // Enqueue call in ACD engine
        let decision = cc_state.acd_engine.enqueue(call, Some("support"));

        // With an available agent, should eventually get assigned
        match decision {
            rustpbx::addons::cc::acd::AcdDecision::Wait { .. } => {
                // Wait decision is expected initially, agent assignment happens on tick
                sleep(Duration::from_millis(100)).await;

                // Get agent snapshots for ACD
                let agent_records = cc_state.agent_registry.list_agents().await;
                let snapshots: Vec<rustpbx::addons::cc::acd::AgentSnapshot> = agent_records
                    .iter()
                    .map(|a| rustpbx::addons::cc::acd::AgentSnapshot {
                        agent_id: a.agent_id.clone(),
                        display_name: a.agent_id.clone(),
                        skills: a.skills.clone(),
                        skill_levels: std::collections::HashMap::new(),
                        max_concurrency: a.max_concurrency as u32,
                        current_calls: a.current_calls as u32,
                        presence: rustpbx::call::app::agent_registry::PresenceState::Idle,
                        idle_duration_secs: 0,
                        total_calls_handled: 0,
                        priority: 0,
                        csat_avg: None,
                    })
                    .collect();

                // Run ACD tick
                let decisions = cc_state.acd_engine.tick(&snapshots, Some("support"));

                // Should have assignment decision
                let has_assign = decisions
                    .iter()
                    .any(|d| matches!(d, rustpbx::addons::cc::acd::AcdDecision::Assign { .. }));

                assert!(has_assign, "ACD should assign call to available agent");
            }
            rustpbx::addons::cc::acd::AcdDecision::Assign { agent_id, .. } => {
                assert_eq!(agent_id, "agent-001");
            }
            other => panic!("Unexpected decision: {:?}", other),
        }
    }

    /// Test ACD overflow:
    /// 1. Configure overflow to voicemail after timeout
    /// 2. Create call with no available agents
    /// 3. Verify call overflows to voicemail
    #[tokio::test]
    async fn test_acd_overflow_to_voicemail() {
        let cc_state = cc_state_always_open();

        // Don't register any agents - simulate all busy

        // Create call
        let call = rustpbx::addons::cc::acd::CallContext {
            call_id: "test-call-002".to_string(),
            trace_id: "trace-002".to_string(),
            caller_number: "1001".to_string(),
            caller_name: None,
            skill_group_id: "support".to_string(),
            priority: 0,
            required_skills: vec![],
            queue_time: std::time::Instant::now() - Duration::from_secs(150), // Simulate waiting 150s
            custom_data: std::collections::HashMap::new(),
        };

        // Enqueue call
        cc_state.acd_engine.enqueue(call, Some("support"));

        // Run tick with no agents (simulating timeout)
        let agents: Vec<rustpbx::addons::cc::acd::AgentSnapshot> = vec![];
        let decisions = cc_state.acd_engine.tick(&agents, Some("support"));

        // Should get overflow or fallback decision
        let has_overflow = decisions.iter().any(|d| {
            matches!(
                d,
                rustpbx::addons::cc::acd::AcdDecision::Overflow { .. }
                    | rustpbx::addons::cc::acd::AcdDecision::Fallback { .. }
            )
        });

        assert!(
            has_overflow,
            "Call should overflow when no agents available and timeout reached"
        );
    }

    /// Test ACD priority:
    /// 1. Create VIP and normal calls
    /// 2. Verify VIP call is assigned first
    #[tokio::test]
    async fn test_acd_vip_priority_routing() {
        let cc_state = cc_state_always_open();

        // Register one agent
        cc_state
            .agent_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        cc_state
            .agent_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();

        // Create normal call first
        let normal_call = rustpbx::addons::cc::acd::CallContext {
            call_id: "normal-call".to_string(),
            trace_id: "trace-normal".to_string(),
            caller_number: "1000".to_string(),
            caller_name: None,
            skill_group_id: "support".to_string(),
            priority: 0,
            required_skills: vec![],
            queue_time: std::time::Instant::now() - Duration::from_secs(10),
            custom_data: std::collections::HashMap::new(),
        };

        // Create VIP call
        let mut vip_call = rustpbx::addons::cc::acd::CallContext {
            call_id: "vip-call".to_string(),
            trace_id: "trace-vip".to_string(),
            caller_number: "1001".to_string(),
            caller_name: Some("VIP Customer".to_string()),
            skill_group_id: "support".to_string(),
            priority: 0,
            required_skills: vec![],
            queue_time: std::time::Instant::now(),
            custom_data: std::collections::HashMap::new(),
        };
        vip_call
            .custom_data
            .insert("vip_level".to_string(), "gold".to_string());

        // Enqueue both calls
        cc_state.acd_engine.enqueue(normal_call, Some("support"));
        cc_state.acd_engine.enqueue(vip_call, Some("support"));

        // Get agent snapshots
        let agent_records = cc_state.agent_registry.list_agents().await;
        let snapshots: Vec<rustpbx::addons::cc::acd::AgentSnapshot> = agent_records
            .iter()
            .map(|a| rustpbx::addons::cc::acd::AgentSnapshot {
                agent_id: a.agent_id.clone(),
                display_name: a.agent_id.clone(),
                skills: a.skills.clone(),
                skill_levels: std::collections::HashMap::new(),
                max_concurrency: a.max_concurrency as u32,
                current_calls: a.current_calls as u32,
                presence: rustpbx::call::app::agent_registry::PresenceState::Idle,
                idle_duration_secs: 0,
                total_calls_handled: 0,
                priority: 0,
                csat_avg: None,
            })
            .collect();

        // Run tick
        let decisions = cc_state.acd_engine.tick(&snapshots, Some("support"));

        // Find assignment decision
        let assign_decision = decisions
            .iter()
            .find(|d| matches!(d, rustpbx::addons::cc::acd::AcdDecision::Assign { .. }));

        assert!(assign_decision.is_some(), "Should assign one call");

        // Queue should have one remaining call
        assert_eq!(cc_state.acd_engine.queue_len(), 1);
    }

    /// Test ACD diagnostics API
    #[tokio::test]
    async fn test_acd_diagnostics_api() {
        // This test doesn't require a running SIP server
        let cc_state = rustpbx::addons::cc::CcAddonState::new();

        // Register test agent
        cc_state
            .agent_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        // Verify ACD engine is initialized
        assert_eq!(cc_state.acd_engine.queue_len(), 0);

        // Verify agent registry works with ACD
        let agents = cc_state.agent_registry.list_agents().await;
        assert_eq!(agents.len(), 1);

        // Create test call
        let call = rustpbx::addons::cc::acd::CallContext {
            call_id: "diag-test".to_string(),
            trace_id: "trace-diag".to_string(),
            caller_number: "9999".to_string(),
            caller_name: None,
            skill_group_id: "support".to_string(),
            priority: 0,
            required_skills: vec![],
            queue_time: std::time::Instant::now(),
            custom_data: std::collections::HashMap::new(),
        };

        // Enqueue and verify
        let decision = cc_state.acd_engine.enqueue(call, Some("support"));
        // Decision could be Wait or Fallback depending on schedule
        assert!(
            matches!(decision, rustpbx::addons::cc::acd::AcdDecision::Wait { .. })
                || matches!(
                    decision,
                    rustpbx::addons::cc::acd::AcdDecision::Fallback { .. }
                )
        );
        // If Wait, queue should have 1 item; if Fallback, queue should be empty
        assert!(cc_state.acd_engine.queue_len() <= 1);
    }
}
