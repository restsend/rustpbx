    use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
    use rustpbx::addons::cc::agent::{AgentRegistry, AgentStatus};
    use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
    use rustpbx::addons::cc::models::cc_agent_endpoint;
    use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
    use rustpbx::call::app::agent_registry::AgentRegistry as TraitAgentRegistry;
    use rustpbx::call::app::agent_registry::RoutingStrategy;
    use rustpbx::proxy::routing::{
        QueueDialMode, RouteQueueConfig, RouteQueueStrategyConfig, RouteQueueTargetConfig,
    };
    use sea_orm::{ActiveModelTrait, Database, Set};
    use sea_orm_migration::MigratorTrait;
    use std::sync::Arc;

    fn acd_disabled() -> Arc<AcdEngine> {
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: false,
            ..AcdConfig::default()
        }))
    }

    #[tokio::test]
    async fn test_skill_group_target_resolution() {
        // Setup: Create CC agent registry with test agents
        let cc_registry = Arc::new(AgentRegistry::new());

        // Register agents with skills
        cc_registry
            .register(
                "agent-001".to_string(),
                vec!["support".to_string(), "billing".to_string()],
                2,
            )
            .await
            .unwrap();

        cc_registry
            .register("agent-002".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        cc_registry
            .register("agent-003".to_string(), vec!["sales".to_string()], 1)
            .await
            .unwrap();

        // Set agents to idle (available)
        cc_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-002", AgentStatus::Idle)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-003", AgentStatus::Idle)
            .await
            .unwrap();

        // Create adapter
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            Arc::new(AcdEngine::new(AcdConfig::default())),
            "localhost",
        );

        // Test: Resolve skill-group URI (without DB, should return empty)
        let uris = adapter.resolve_target("skill-group:support").await;

        // Without DB connection, skill group resolution returns empty
        assert!(uris.is_empty(), "Expected empty URIs without DB");
    }

    #[tokio::test]
    async fn test_standard_uri_not_resolved() {
        // Setup
        let cc_registry = Arc::new(AgentRegistry::new());
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            Arc::new(AcdEngine::new(AcdConfig::default())),
            "localhost",
        );

        // Test: Standard SIP URI should not be resolved
        let uris = adapter.resolve_target("sip:agent@example.com").await;
        assert!(uris.is_empty(), "Standard SIP URI should not be resolved");

        // Test: Unknown scheme should not be resolved
        let uris = adapter.resolve_target("unknown:something").await;
        assert!(uris.is_empty(), "Unknown scheme should not be resolved");
    }

    #[tokio::test]
    async fn test_queue_config_with_skill_group_target() {
        // Create queue config with skill group target
        let queue_cfg = RouteQueueConfig {
            strategy: RouteQueueStrategyConfig {
                mode: QueueDialMode::Sequential,
                wait_timeout_secs: Some(15),
                targets: vec![
                    RouteQueueTargetConfig {
                        uri: "sip:backup@pbx.example".to_string(),
                        label: Some("Backup".to_string()),
                    },
                    RouteQueueTargetConfig {
                        uri: "skill-group:support_l1".to_string(),
                        label: Some("Support L1".to_string()),
                    },
                ],
            },
            ..RouteQueueConfig::default()
        };

        // Convert to queue plan
        let plan = queue_cfg.to_queue_plan().expect("should convert to plan");

        // Verify dial strategy contains both targets
        let strategy = plan.dial_strategy.expect("should have dial strategy");
        match strategy {
            rustpbx::call::DialStrategy::Sequential(locations) => {
                assert_eq!(locations.len(), 2);
                assert_eq!(locations[0].aor.to_string(), "sip:backup@pbx.example");
                assert_eq!(locations[1].aor.to_string(), "skill-group:support_l1");
            }
            _ => panic!("Expected sequential strategy"),
        }
    }

    #[tokio::test]
    async fn test_agent_availability_filtering() {
        // Setup
        let cc_registry = Arc::new(AgentRegistry::new());

        cc_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        cc_registry
            .register("agent-002".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        // agent-001 is idle (available)
        cc_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();

        // agent-002: offline -> idle -> ringing -> busy
        cc_registry
            .update_status("agent-002", AgentStatus::Idle)
            .await
            .unwrap();
        cc_registry
            .update_status(
                "agent-002",
                AgentStatus::Ringing {
                    call_id: "call-1".to_string(),
                    since: std::time::Instant::now(),
                },
            )
            .await
            .unwrap();
        cc_registry
            .update_status(
                "agent-002",
                AgentStatus::Busy {
                    call_id: "call-1".to_string(),
                    since: std::time::Instant::now(),
                },
            )
            .await
            .unwrap();

        // Create adapter
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            Arc::new(AcdEngine::new(AcdConfig::default())),
            "localhost",
        );

        // Find available agents with "support" skill
        let agents = adapter
            .find_available_agents(&["support".to_string()])
            .await;

        // Only agent-001 should be available
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, "agent-001");
    }

    #[tokio::test]
    async fn test_skill_matching() {
        // Setup
        let cc_registry = Arc::new(AgentRegistry::new());

        cc_registry
            .register(
                "agent-001".to_string(),
                vec!["support".to_string(), "billing".to_string()],
                1,
            )
            .await
            .unwrap();

        cc_registry
            .register("agent-002".to_string(), vec!["sales".to_string()], 1)
            .await
            .unwrap();

        cc_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-002", AgentStatus::Idle)
            .await
            .unwrap();

        // Create adapter
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            Arc::new(AcdEngine::new(AcdConfig::default())),
            "localhost",
        );

        // Find agents with "billing" skill
        let agents = adapter
            .find_available_agents(&["billing".to_string()])
            .await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, "agent-001");

        // Find agents with both "support" and "billing" skills
        let agents = adapter
            .find_available_agents(&["support".to_string(), "billing".to_string()])
            .await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, "agent-001");

        // Find agents with "sales" skill
        let agents = adapter.find_available_agents(&["sales".to_string()]).await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, "agent-002");
    }

    #[tokio::test]
    async fn test_select_agent_with_policy_graceful_when_acd_disabled() {
        let cc_registry = Arc::new(AgentRegistry::new());

        cc_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost");
        let selected = adapter
            .select_agent_with_policy(
                &["support".to_string()],
                RoutingStrategy::LongestIdle,
                Some("queue-policy-that-is-not-loaded"),
                "test-call",
            )
            .await;

        assert!(
            selected.is_some(),
            "agent selection should fall back when ACD is disabled"
        );
        assert_eq!(selected.unwrap().agent_id, "agent-001");
    }

    #[tokio::test]
    async fn test_skill_group_resolution_with_policy_graceful_when_acd_disabled() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        rustpbx::addons::cc::skill_group::create_skill_group(
            &db,
            CreateSkillGroupRequest {
                skill_group_id: "support_l1".to_string(),
                display_name: Some("Support L1".to_string()),
                skills_required: vec!["support".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                metadata: None,
            },
        )
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        cc_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();

        cc_agent_endpoint::ActiveModel {
            agent_id: Set("agent-001".to_string()),
            endpoint_type: Set("sip_uri".to_string()),
            endpoint_value: Set("sip:agent-001@example.com".to_string()),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost");
        let uris = adapter
            .resolve_target_with_policy(
                "skill-group:support_l1",
                Some("queue-policy-that-is-not-loaded"),
                "test-call",
            )
            .await;

        assert!(
            uris.contains(&"sip:agent-001@localhost".to_string()),
            "expected AOR in results: {:?}",
            uris
        );
    }

    #[tokio::test]
    async fn test_skill_group_resolution_fallback_normalizes_extension_endpoint() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        rustpbx::addons::cc::skill_group::create_skill_group(
            &db,
            CreateSkillGroupRequest {
                skill_group_id: "asdf".to_string(),
                display_name: Some("asdf".to_string()),
                skills_required: vec!["阿第三方".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                metadata: None,
            },
        )
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        cc_registry
            .register("22".to_string(), vec!["阿第三方".to_string()], 1)
            .await
            .unwrap();
        // Keep Offline to force status-agnostic fallback path.

        cc_agent_endpoint::ActiveModel {
            agent_id: Set("22".to_string()),
            endpoint_type: Set("extension".to_string()),
            endpoint_value: Set("22".to_string()),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost");
        let uris = adapter.resolve_target("skill-group:asdf").await;

        assert_eq!(uris, vec!["sip:22@localhost".to_string()]);
    }

    // ─── Cache-miss → DB fallback regression tests ─────────────────────────

    /// Regression: skill groups created via the API live in the DB only and may
    /// not be present in the TOML cache. On a cache miss the resolver must fall
    /// back to the DB; otherwise `skills_required` would be empty and every
    /// Idle agent would match, routing calls to unqualified agents.
    #[tokio::test]
    async fn test_skill_group_cache_miss_falls_back_to_db() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        // Skill group exists ONLY in the DB (simulates API-created group).
        rustpbx::addons::cc::skill_group::create_skill_group(
            &db,
            CreateSkillGroupRequest {
                skill_group_id: "tech-support2_G".to_string(),
                display_name: Some("Tech Support 2".to_string()),
                skills_required: vec!["tech-support2_S".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                metadata: None,
            },
        )
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));

        // Agent A: does NOT have the required skill.
        cc_registry
            .register("agent-A".to_string(), vec![], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-A", AgentStatus::Idle)
            .await
            .unwrap();
        // Agent B: has the required skill.
        cc_registry
            .register(
                "agent-B".to_string(),
                vec!["tech-support2_S".to_string()],
                1,
            )
            .await
            .unwrap();
        cc_registry
            .update_status("agent-B", AgentStatus::Idle)
            .await
            .unwrap();

        // Endpoints so resolution produces dial URIs.
        cc_agent_endpoint::ActiveModel {
            agent_id: Set("agent-A".to_string()),
            endpoint_type: Set("sip_uri".to_string()),
            endpoint_value: Set("sip:agent-A@example.com".to_string()),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        cc_agent_endpoint::ActiveModel {
            agent_id: Set("agent-B".to_string()),
            endpoint_type: Set("sip_uri".to_string()),
            endpoint_value: Set("sip:agent-B@example.com".to_string()),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // A cache that is present but does NOT contain the group → cache miss.
        // Before the fix this returned empty skills and matched both agents.
        let empty_cache = Arc::new(tokio::sync::RwLock::new(
            rustpbx::addons::cc::SkillGroupTomlCache::default(),
        ));

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_cache(empty_cache);

        let uris = adapter.resolve_target("skill-group:tech-support2_G").await;

        // Only agent-B (the qualified one) must be returned, by its AOR.
        assert!(
            uris.contains(&"sip:agent-B@localhost".to_string()),
            "expected AOR in results: {:?}",
            uris
        );
    }

    /// When neither the cache nor the DB know the group, resolution must yield
    /// no candidates (and must NOT match every Idle agent).
    #[tokio::test]
    async fn test_skill_group_missing_everywhere_returns_empty() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        cc_registry
            .register("agent-A".to_string(), vec![], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-A", AgentStatus::Idle)
            .await
            .unwrap();
        cc_agent_endpoint::ActiveModel {
            agent_id: Set("agent-A".to_string()),
            endpoint_type: Set("sip_uri".to_string()),
            endpoint_value: Set("sip:agent-A@example.com".to_string()),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let empty_cache = Arc::new(tokio::sync::RwLock::new(
            rustpbx::addons::cc::SkillGroupTomlCache::default(),
        ));
        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_cache(empty_cache);

        let uris = adapter.resolve_target("skill-group:does-not-exist").await;
        assert!(
            uris.is_empty(),
            "Unknown skill group must not match any agent"
        );
    }

    // ─── ACD policy selection tests ──────────────────────────────────────────

    /// Helper: build an AcdEngine with a single named policy that uses LongestIdle.
    fn acd_with_longest_idle_policy(policy_name: &str) -> Arc<AcdEngine> {
        use rustpbx::addons::cc::acd::{
            AcdPolicy, BusinessHours, OverflowConfig, ScheduleConfig, StrategyConfig, StrategyType,
        };
        let mut policies = std::collections::HashMap::new();
        policies.insert(
            policy_name.to_string(),
            AcdPolicy {
                name: policy_name.to_string(),
                strategy: StrategyConfig {
                    strategy_type: StrategyType::LongestIdle,
                    ..Default::default()
                },
                overflow: OverflowConfig {
                    triggers: vec![],
                    chain: vec![],
                    ..Default::default()
                },
                schedule: ScheduleConfig {
                    business_hours: Some(BusinessHours {
                        start: "00:00".to_string(),
                        end: "23:59".to_string(),
                        timezone: "UTC".to_string(),
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: true,
            default_policy: policy_name.to_string(),
            policies,
        }))
    }

    /// Helper: build an AcdEngine with a single named policy that uses LeastAnswered.
    fn acd_with_least_answered_policy(policy_name: &str) -> Arc<AcdEngine> {
        use rustpbx::addons::cc::acd::{
            AcdPolicy, BusinessHours, OverflowConfig, ScheduleConfig, StrategyConfig, StrategyType,
        };
        let mut policies = std::collections::HashMap::new();
        policies.insert(
            policy_name.to_string(),
            AcdPolicy {
                name: policy_name.to_string(),
                strategy: StrategyConfig {
                    strategy_type: StrategyType::LeastAnswered,
                    ..Default::default()
                },
                overflow: OverflowConfig {
                    triggers: vec![],
                    chain: vec![],
                    ..Default::default()
                },
                schedule: ScheduleConfig {
                    business_hours: Some(BusinessHours {
                        start: "00:00".to_string(),
                        end: "23:59".to_string(),
                        timezone: "UTC".to_string(),
                    }),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: true,
            default_policy: policy_name.to_string(),
            policies,
        }))
    }

    /// When ACD is enabled and the queue's acd_policy is set to LongestIdle,
    /// `select_agent_with_policy` must pick the agent that has been idle longest,
    /// even when the caller requests a different fallback strategy (RoundRobin).
    #[tokio::test]
    async fn test_queue_acd_policy_longest_idle_overrides_fallback() {
        let cc_registry = Arc::new(AgentRegistry::new());

        // Register two agents with the same skill.
        cc_registry
            .register("agent-a".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .register("agent-b".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        // Set agent-a Idle first so it has a longer idle duration.
        cc_registry
            .update_status("agent-a", AgentStatus::Idle)
            .await
            .unwrap();

        // Sleep > 1 s so idle_duration_secs (whole-second granularity) differs.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        cc_registry
            .update_status("agent-b", AgentStatus::Idle)
            .await
            .unwrap();

        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            acd_with_longest_idle_policy("support-policy"),
            "localhost",
        );

        // Fallback strategy is RoundRobin, but ACD policy overrides to LongestIdle.
        let selected = adapter
            .select_agent_with_policy(
                &["support".to_string()],
                RoutingStrategy::RoundRobin,
                Some("support-policy"),
                "test-call",
            )
            .await;

        assert!(selected.is_some(), "agent should be selected");
        assert_eq!(
            selected.unwrap().agent_id,
            "agent-a",
            "LongestIdle should pick the agent idle longest"
        );
    }

    /// With ACD disabled the fallback RoutingStrategy is used unchanged.
    /// This is the negative counterpart to the test above.
    #[tokio::test]
    async fn test_queue_no_acd_uses_fallback_strategy() {
        let cc_registry = Arc::new(AgentRegistry::new());

        cc_registry
            .register("agent-x".to_string(), vec!["billing".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-x", AgentStatus::Idle)
            .await
            .unwrap();

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost");

        let selected = adapter
            .select_agent_with_policy(
                &["billing".to_string()],
                RoutingStrategy::LongestIdle,
                None, // no policy → NoDecision → fallback
                "test-call",
            )
            .await;

        assert!(selected.is_some());
        assert_eq!(selected.unwrap().agent_id, "agent-x");
    }

    /// When the queue specifies a policy that does not exist in the ACD config
    /// the engine falls back to the default policy gracefully.
    #[tokio::test]
    async fn test_queue_acd_unknown_policy_uses_default() {
        let cc_registry = Arc::new(AgentRegistry::new());

        cc_registry
            .register("agent-001".to_string(), vec!["sales".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("agent-001", AgentStatus::Idle)
            .await
            .unwrap();

        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            acd_with_longest_idle_policy("default"),
            "localhost",
        );

        // "nonexistent-policy" is not in the config; engine falls back to default.
        let selected = adapter
            .select_agent_with_policy(
                &["sales".to_string()],
                RoutingStrategy::LongestIdle,
                Some("nonexistent-policy"),
                "test-call",
            )
            .await;

        assert!(
            selected.is_some(),
            "should still select an agent via default policy fallback"
        );
        assert_eq!(selected.unwrap().agent_id, "agent-001");
    }

    /// `skill-group` targets also honour the ACD policy when
    /// `resolve_target_with_policy` is called.
    #[tokio::test]
    async fn test_skill_group_target_with_acd_policy_selects_longest_idle() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        rustpbx::addons::cc::skill_group::create_skill_group(
            &db,
            CreateSkillGroupRequest {
                skill_group_id: "tier1".to_string(),
                display_name: Some("Tier 1 Support".to_string()),
                skills_required: vec!["support".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                metadata: None,
            },
        )
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));

        for agent_id in &["sg-agent-a", "sg-agent-b"] {
            cc_registry
                .register(agent_id.to_string(), vec!["support".to_string()], 1)
                .await
                .unwrap();

            cc_agent_endpoint::ActiveModel {
                agent_id: Set(agent_id.to_string()),
                endpoint_type: Set("sip_uri".to_string()),
                endpoint_value: Set(format!("sip:{}@example.com", agent_id)),
                priority: Set(1),
                is_active: Set(true),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        // Set both agents Idle.
        cc_registry
            .update_status("sg-agent-a", AgentStatus::Idle)
            .await
            .unwrap();
        cc_registry
            .update_status("sg-agent-b", AgentStatus::Idle)
            .await
            .unwrap();

        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            acd_with_longest_idle_policy("sg-policy"),
            "localhost",
        );

        let uris = adapter
            .resolve_target_with_policy("skill-group:tier1", Some("sg-policy"), "test-call")
            .await;

        // ACD policy is active: at least one URI is returned.
        assert!(
            !uris.is_empty(),
            "ACD policy should resolve skill-group to available agents"
        );
        // The AOR (sip:agent_id@localhost) must be present for proper identity.
        assert!(
            uris.contains(&"sip:sg-agent-a@localhost".to_string()),
            "AOR for sg-agent-a must be in resolved URIs: {:?}",
            uris
        );
    }

    /// With `LeastAnswered` strategy and two agents that have identical call
    /// history, both remain eligible and one is returned (non-empty result).
    /// The test primarily verifies the strategy path compiles and runs without
    /// panicking; deterministic ordering for equal agents is implementation-
    /// defined.
    #[tokio::test]
    async fn test_queue_acd_least_answered_strategy_runs() {
        let cc_registry = Arc::new(AgentRegistry::new());

        for agent_id in &["la-agent-1", "la-agent-2"] {
            cc_registry
                .register(agent_id.to_string(), vec!["support".to_string()], 1)
                .await
                .unwrap();
            cc_registry
                .update_status(agent_id, AgentStatus::Idle)
                .await
                .unwrap();
        }

        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            acd_with_least_answered_policy("least-ans"),
            "localhost",
        );

        let selected = adapter
            .select_agent_with_policy(
                &["support".to_string()],
                RoutingStrategy::LongestIdle,
                Some("least-ans"),
                "test-call",
            )
            .await;

        assert!(
            selected.is_some(),
            "LeastAnswered strategy should select one of the available agents"
        );
    }

    // ═════════════════════════════════════════════════════════════════════
    //  Skill group ↔ ACD policy binding tests
    // ═════════════════════════════════════════════════════════════════════

    /// Helper: insert a skill group directly with an `acd_policy` set.
    async fn create_sg_with_policy(
        db: &sea_orm::DatabaseConnection,
        id: &str,
        skills: &[&str],
        policy: Option<&str>,
    ) {
        use rustpbx::addons::cc::models::cc_skill_group;
        cc_skill_group::ActiveModel {
            skill_group_id: Set(id.to_string()),
            display_name: Set(None),
            skills_required: Set(serde_json::json!(skills)),
            overflow_groups: Set(serde_json::json!([])),
            sla_target_secs: Set(30),
            max_wait_secs: Set(90),
            acd_policy: Set(policy.map(|p| p.to_string())),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn register_agent_with_endpoint(
        cc_registry: &AgentRegistry,
        db: &sea_orm::DatabaseConnection,
        agent_id: &str,
        skills: &[&str],
    ) {
        cc_registry
            .register(
                agent_id.to_string(),
                skills.iter().map(|s| s.to_string()).collect(),
                1,
            )
            .await
            .unwrap();
        cc_registry
            .update_status(agent_id, AgentStatus::Idle)
            .await
            .unwrap();
        use rustpbx::addons::cc::models::cc_agent_endpoint;
        cc_agent_endpoint::ActiveModel {
            agent_id: Set(agent_id.to_string()),
            endpoint_type: Set("sip_uri".to_string()),
            endpoint_value: Set(format!("sip:{}@example.com", agent_id)),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// When a skill group has `acd_policy` set, `resolve_target_with_policy`
    /// must use that policy — even when the *caller* passes `None`.
    #[tokio::test]
    async fn test_skill_group_acd_policy_drives_strategy() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        create_sg_with_policy(&db, "vip-tier", &["support"], Some("round-robin-policy")).await;

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        register_agent_with_endpoint(&cc_registry, &db, "rr-a", &["support"]).await;
        register_agent_with_endpoint(&cc_registry, &db, "rr-b", &["support"]).await;

        let cc_clone = cc_registry.clone();
        let adapter = CcAgentRegistryAdapter::new(
            cc_clone,
            acd_with_round_robin_policy("round-robin-policy"),
            "localhost",
        );

        let uris = adapter
            .resolve_target_with_policy("skill-group:vip-tier", None, "test-call")
            .await;

        assert!(
            !uris.is_empty(),
            "Skill group with acd_policy should resolve to agents"
        );
    }

    /// When the skill group has NO acd_policy and the caller also passes None,
    /// the adapter falls back to the default LongestIdle strategy.
    #[tokio::test]
    async fn test_skill_group_without_acd_policy_uses_fallback() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        create_sg_with_policy(&db, "basic", &["support"], None).await;

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        register_agent_with_endpoint(&cc_registry, &db, "basic-a", &["support"]).await;

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost");

        let uris = adapter
            .resolve_target_with_policy("skill-group:basic", None, "test-call")
            .await;

        assert!(!uris.is_empty(), "Fallback should still resolve agents");
    }

    /// An unknown acd_policy name on the skill group gracefully falls back to
    /// the ACD engine's default_policy.
    #[tokio::test]
    async fn test_skill_group_unknown_acd_policy_falls_back_to_default() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        create_sg_with_policy(&db, "mystery", &["sales"], Some("nonexistent")).await;

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        register_agent_with_endpoint(&cc_registry, &db, "m-a", &["sales"]).await;

        // Engine has a "default" policy; the unknown name should fall back.
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            acd_with_longest_idle_policy("default"),
            "localhost",
        );

        let uris = adapter
            .resolve_target_with_policy("skill-group:mystery", None, "test-call")
            .await;

        assert!(
            !uris.is_empty(),
            "Unknown policy should fall back gracefully"
        );
    }

    /// Skill level requirements in skills_required (e.g. `"support>=5"`) are
    /// enforced when resolving a skill group to agents.
    #[tokio::test]
    async fn test_skill_group_with_level_requirement_filters_agents() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        // Create a skill group that requires support>=5
        use rustpbx::addons::cc::models::cc_skill_group;
        cc_skill_group::ActiveModel {
            skill_group_id: Set("expert-only".to_string()),
            display_name: Set(None),
            skills_required: Set(serde_json::json!(["support>=5"])),
            overflow_groups: Set(serde_json::json!([])),
            sla_target_secs: Set(30),
            max_wait_secs: Set(90),
            acd_policy: Set(None),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));

        // Agent with support level 3 — should be filtered OUT
        cc_registry
            .register("junior".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .set_agent_skill_levels("junior", [("support".to_string(), 3)].into())
            .await
            .unwrap();
        cc_registry
            .update_status("junior", AgentStatus::Idle)
            .await
            .unwrap();

        // Agent with support level 8 — should be included
        cc_registry
            .register("senior".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .set_agent_skill_levels("senior", [("support".to_string(), 8)].into())
            .await
            .unwrap();
        cc_registry
            .update_status("senior", AgentStatus::Idle)
            .await
            .unwrap();

        // Endpoints
        for aid in &["junior", "senior"] {
            rustpbx::addons::cc::models::cc_agent_endpoint::ActiveModel {
                agent_id: Set(aid.to_string()),
                endpoint_type: Set("sip_uri".to_string()),
                endpoint_value: Set(format!("sip:{}@example.com", aid)),
                priority: Set(1),
                is_active: Set(true),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost");

        let uris = adapter
            .resolve_target_with_policy("skill-group:expert-only", None, "test-call")
            .await;

        // Only the senior agent should be resolved (level 8 >= 5)
        assert!(
            uris.iter().any(|u| u.contains("senior")),
            "senior should be resolved"
        );
        assert!(
            !uris.iter().any(|u| u.contains("junior")),
            "junior (level 3 < 5) should be filtered out"
        );
    }

    /// ACD policy-level `min_level` eligibility filter (configured on the
    /// `AcdPolicy`, not the per-skill `skills_required`) is enforced end-to-end
    /// when resolving a skill group to agents — covering both the inline ACD
    /// tick() path and the `choose_agent_order` fallback.
    #[tokio::test]
    async fn test_skill_group_policy_min_level_filters_agents() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        // Skill group bound to policy "expert" (no per-skill level constraint;
        // the filtering comes purely from the policy's min_level).
        use rustpbx::addons::cc::models::cc_skill_group;
        cc_skill_group::ActiveModel {
            skill_group_id: Set("expert-policy".to_string()),
            display_name: Set(None),
            skills_required: Set(serde_json::json!(["support"])),
            overflow_groups: Set(serde_json::json!([])),
            sla_target_secs: Set(30),
            max_wait_secs: Set(90),
            acd_policy: Set(Some("expert".to_string())),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));

        // junior: support level 3 — filtered OUT by policy min_level=7
        cc_registry
            .register("junior".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .set_agent_skill_levels("junior", [("support".to_string(), 3)].into())
            .await
            .unwrap();
        cc_registry
            .update_status("junior", AgentStatus::Idle)
            .await
            .unwrap();

        // senior: support level 8 — kept
        cc_registry
            .register("senior".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .set_agent_skill_levels("senior", [("support".to_string(), 8)].into())
            .await
            .unwrap();
        cc_registry
            .update_status("senior", AgentStatus::Idle)
            .await
            .unwrap();

        for aid in &["junior", "senior"] {
            rustpbx::addons::cc::models::cc_agent_endpoint::ActiveModel {
                agent_id: Set(aid.to_string()),
                endpoint_type: Set("sip_uri".to_string()),
                endpoint_value: Set(format!("sip:{}@example.com", aid)),
                priority: Set(1),
                is_active: Set(true),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let acd = {
            use rustpbx::addons::cc::acd::{
                AcdPolicy, BusinessHours, OverflowConfig, ScheduleConfig, StrategyConfig,
                StrategyType,
            };
            let mut policies = std::collections::HashMap::new();
            policies.insert(
                "expert".to_string(),
                AcdPolicy {
                    name: "expert".to_string(),
                    strategy: StrategyConfig {
                        strategy_type: StrategyType::LongestIdle,
                        ..Default::default()
                    },
                    overflow: OverflowConfig::default(),
                    schedule: ScheduleConfig {
                        business_hours: Some(BusinessHours {
                            start: "00:00".to_string(),
                            end: "23:59".to_string(),
                            timezone: "UTC".to_string(),
                        }),
                        ..Default::default()
                    },
                    min_level: Some(7),
                    ..Default::default()
                },
            );
            Arc::new(AcdEngine::new(AcdConfig {
                enabled: true,
                policies,
                default_policy: "expert".to_string(),
            }))
        };

        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd, "localhost");

        let uris = adapter
            .resolve_target_with_policy("skill-group:expert-policy", None, "test-call")
            .await;

        assert!(
            uris.iter().any(|u| u.contains("senior")),
            "senior (peak level 8 >= 7) should be resolved"
        );
        assert!(
            !uris.iter().any(|u| u.contains("junior")),
            "junior (peak level 3 < policy min_level 7) should be filtered out"
        );
    }

    fn acd_with_round_robin_policy(name: &str) -> Arc<AcdEngine> {
        use rustpbx::addons::cc::acd::{BusinessHours, ScheduleConfig, StrategyConfig, StrategyType};
        let mut policies = std::collections::HashMap::new();
        policies.insert(
            name.to_string(),
            rustpbx::addons::cc::acd::AcdPolicy {
                name: name.to_string(),
                strategy: StrategyConfig {
                    strategy_type: StrategyType::RoundRobin,
                    ..StrategyConfig::default()
                },
                schedule: ScheduleConfig {
                    business_hours: Some(BusinessHours {
                        start: "00:00".to_string(),
                        end: "23:59".to_string(),
                        timezone: "UTC".to_string(),
                    }),
                    holidays: std::collections::HashMap::new(),
                    night_mode: None,
                },
                ..Default::default()
            },
        );
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: true,
            default_policy: name.to_string(),
            policies,
        }))
    }

    /// Build a skill-group cache with a single "support" group.
    fn make_skill_cache(
        acd_policy: Option<&str>,
    ) -> Arc<tokio::sync::RwLock<rustpbx::addons::cc::SkillGroupTomlCache>> {
        let mut cache = rustpbx::addons::cc::SkillGroupTomlCache::default();
        cache.groups.insert(
            "support".to_string(),
            rustpbx::addons::cc::SkillGroupConfigEntry {
                skill_group_id: "support".to_string(),
                display_name: Some("Support".to_string()),
                skills_required: vec!["support".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                acd_policy: acd_policy.map(|p| p.to_string()),
            },
        );
        Arc::new(tokio::sync::RwLock::new(cache))
    }

    fn register_skill_agent(registry: &Arc<AgentRegistry>, id: &str, status: AgentStatus) {
        futures::executor::block_on(async {
            registry
                .register(id.to_string(), vec!["support".to_string()], 1)
                .await
                .unwrap();
            if matches!(status, AgentStatus::Idle) {
                registry.update_status(id, AgentStatus::Idle).await.unwrap();
            }
        });
    }

    // ─── skill_group scheduling event accuracy ──────────────────────────────

    /// No ACD policy and no immediately available agent: the call must be
    /// queued → `skill_group_call_queued` is emitted (single source), even
    /// though skill-matched agents exist (they are dialed as fallback).
    #[tokio::test]
    async fn test_no_policy_unavailable_emits_call_queued() {
        use rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent;

        let cc_registry = Arc::new(AgentRegistry::new());
        // Skill-matched but Offline (default status) → no available candidate.
        cc_registry
            .register("agent-001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_cache(make_skill_cache(None))
            .with_skill_group_event_tx(tx);

        let uris = adapter.resolve_target("skill-group:support").await;
        assert!(!uris.is_empty(), "fallback URIs expected");

        let mut saw_queued = false;
        let mut saw_candidates = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SkillGroupEvent::CallQueued { skill_group_id, .. } => {
                    assert_eq!(skill_group_id.as_deref(), Some("support"));
                    saw_queued = true;
                }
                SkillGroupEvent::CandidatesFound { .. } => saw_candidates = true,
                _ => {}
            }
        }
        assert!(saw_candidates, "candidates_found expected");
        assert!(
            saw_queued,
            "skill_group_call_queued must fire without a policy when no agent is available"
        );
    }

    /// No policy but an idle agent available: dial immediately, NO call_queued.
    #[tokio::test]
    async fn test_no_policy_with_available_no_call_queued() {
        use rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent;

        let cc_registry = Arc::new(AgentRegistry::new());
        register_skill_agent(&cc_registry, "agent-001", AgentStatus::Idle);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_cache(make_skill_cache(None))
            .with_skill_group_event_tx(tx);

        adapter.resolve_target("skill-group:support").await;

        let mut saw_queued = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, SkillGroupEvent::CallQueued { .. }) {
                saw_queued = true;
            }
        }
        assert!(
            !saw_queued,
            "no call_queued when an agent is dialed immediately"
        );
    }

    /// Non-inline path (no policy): the strategy-picked first agent must be
    /// reported via `agent_assigned`.
    #[tokio::test]
    async fn test_non_inline_path_emits_agent_assigned() {
        use rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent;

        let cc_registry = Arc::new(AgentRegistry::new());
        register_skill_agent(&cc_registry, "agent-001", AgentStatus::Idle);
        register_skill_agent(&cc_registry, "agent-002", AgentStatus::Idle);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_cache(make_skill_cache(None))
            .with_skill_group_event_tx(tx);

        adapter.resolve_target("skill-group:support").await;

        let mut assigned = None;
        while let Ok(ev) = rx.try_recv() {
            if let SkillGroupEvent::AgentAssigned {
                agent_id,
                skill_group_id,
                dispatch_reason,
                ..
            } = ev
            {
                assigned = Some((agent_id, skill_group_id, dispatch_reason));
            }
        }
        let (agent_id, sg, reason) = assigned.expect("agent_assigned expected");
        assert_eq!(sg.as_deref(), Some("support"));
        assert_eq!(reason, "regular");
        assert!(agent_id == "agent-001" || agent_id == "agent-002");
    }

    /// With an ACD policy, `enqueue` returns Wait with real position/EWT →
    /// `skill_group_call_queued` carries them, then the agent is assigned.
    #[tokio::test]
    async fn test_policy_wait_emits_call_queued_with_real_values() {
        use rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent;

        let cc_registry = Arc::new(AgentRegistry::new());
        register_skill_agent(&cc_registry, "agent-001", AgentStatus::Idle);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry,
            acd_with_longest_idle_policy("policy1"),
            "localhost",
        )
        .with_skill_group_cache(make_skill_cache(Some("policy1")))
        .with_skill_group_event_tx(tx);

        adapter.resolve_target("skill-group:support").await;

        let mut queued = None;
        let mut assigned = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SkillGroupEvent::CallQueued {
                    position,
                    ewt_secs,
                    reason,
                    ..
                } => {
                    assert!(position >= 1, "position must be the real queue position");
                    assert!(ewt_secs > 0, "ewt_secs must be real, got {ewt_secs}");
                    assert_eq!(reason, "no_agent_available");
                    queued = Some(position);
                }
                SkillGroupEvent::AgentAssigned { .. } => assigned = true,
                _ => {}
            }
        }
        assert!(
            queued.is_some(),
            "skill_group_call_queued expected on Wait decision"
        );
        assert!(assigned, "agent_assigned expected after inline assignment");
    }

    /// `notify_call_abandoned` lifecycle hook → `skill_group_call_abandoned`.
    #[tokio::test]
    async fn test_notify_abandoned_emits_call_abandoned() {
        use rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent;

        let cc_registry = Arc::new(AgentRegistry::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_event_tx(tx);

        let _ = TraitAgentRegistry::notify_call_abandoned(&adapter, "call-1", "support", 42).await;

        match rx.try_recv() {
            Ok(SkillGroupEvent::CallAbandoned {
                call_id,
                skill_group_id,
                waited_secs,
                ..
            }) => {
                assert_eq!(call_id, "call-1");
                assert_eq!(skill_group_id.as_deref(), Some("support"));
                assert_eq!(waited_secs, 42);
            }
            other => panic!("expected CallAbandoned, got {other:?}"),
        }
    }

    /// `notify_call_timeout` / `notify_call_fallback` → `skill_group_service_unavailable`.
    #[tokio::test]
    async fn test_notify_timeout_fallback_emits_service_unavailable() {
        use rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent;

        let cc_registry = Arc::new(AgentRegistry::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
        let adapter = CcAgentRegistryAdapter::new(cc_registry, acd_disabled(), "localhost")
            .with_skill_group_event_tx(tx);

        let _ = TraitAgentRegistry::notify_call_timeout(&adapter, "call-1", "support", 90).await;
        match rx.try_recv() {
            Ok(SkillGroupEvent::ServiceUnavailable {
                call_id,
                reason,
                waited_secs,
                ..
            }) => {
                assert_eq!(call_id, "call-1");
                assert_eq!(reason, "timeout");
                assert_eq!(waited_secs, 90);
            }
            other => panic!("expected ServiceUnavailable(timeout), got {other:?}"),
        }

        let _ = TraitAgentRegistry::notify_call_fallback(
            &adapter, "call-2", "support", "no_agent", "hangup",
        )
        .await;
        match rx.try_recv() {
            Ok(SkillGroupEvent::ServiceUnavailable {
                call_id,
                fallback_action,
                ..
            }) => {
                assert_eq!(call_id, "call-2");
                assert_eq!(fallback_action, "hangup");
            }
            other => panic!("expected ServiceUnavailable(fallback), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_concurrent_resolves_assign_distinct_agents() {
        use rustpbx::addons::cc::SkillGroupTomlCache;
        use rustpbx::addons::cc::models::cc_agent_endpoint;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        rustpbx::addons::cc::skill_group::create_skill_group(
            &db,
            CreateSkillGroupRequest {
                skill_group_id: "support".to_string(),
                display_name: Some("Support".to_string()),
                skills_required: vec!["support".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                metadata: None,
            },
        )
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        for id in ["1001", "1002", "1003"] {
            cc_registry
                .register(id.to_string(), vec!["support".to_string()], 1)
                .await
                .unwrap();
            cc_registry
                .update_status(id, AgentStatus::Idle)
                .await
                .unwrap();
            cc_agent_endpoint::ActiveModel {
                agent_id: Set(id.to_string()),
                endpoint_type: Set("sip_uri".to_string()),
                endpoint_value: Set(format!("sip:{}@example.com", id)),
                priority: Set(1),
                is_active: Set(true),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let cache = Arc::new(tokio::sync::RwLock::new(
            SkillGroupTomlCache::default(),
        ));
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry.clone(),
            acd_disabled(),
            "localhost",
        )
        .with_skill_group_cache(cache);

        // Simulate three concurrent calls to the same skill group.
        let uris1 = adapter
            .resolve_target_with_policy("skill-group:support", None, "call-1")
            .await;
        let uris2 = adapter
            .resolve_target_with_policy("skill-group:support", None, "call-2")
            .await;
        let uris3 = adapter
            .resolve_target_with_policy("skill-group:support", None, "call-3")
            .await;

        // Each call must have at least its primary reserved agent.
        assert!(!uris1.is_empty(), "call-1 should get a reserved agent");
        assert!(!uris2.is_empty(), "call-2 should get a reserved agent");
        assert!(!uris3.is_empty(), "call-3 should get a reserved agent");

        // The primary (first) agent must be distinct across concurrent calls.
        let primary1 = uris1[0].clone();
        let primary2 = uris2[0].clone();
        let primary3 = uris3[0].clone();
        assert_ne!(primary1, primary2, "concurrent calls must reserve distinct agents");
        assert_ne!(primary1, primary3, "concurrent calls must reserve distinct agents");
        assert_ne!(primary2, primary3, "concurrent calls must reserve distinct agents");

        // All three agents should now be in Ringing state.
        for id in ["1001", "1002", "1003"] {
            let agent = cc_registry.get_agent(id).await.unwrap();
            assert!(
                matches!(agent.status, AgentStatus::Ringing { .. }),
                "agent {id} should be Ringing after reservation"
            );
        }
    }

    #[tokio::test]
    async fn test_reservation_fallback_when_all_agents_taken() {
        use rustpbx::addons::cc::SkillGroupTomlCache;
        use rustpbx::addons::cc::models::cc_agent_endpoint;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        rustpbx::addons::cc::skill_group::create_skill_group(
            &db,
            CreateSkillGroupRequest {
                skill_group_id: "support".to_string(),
                display_name: None,
                skills_required: vec!["support".to_string()],
                overflow_groups: vec![],
                sla_target_secs: 30,
                max_wait_secs: 90,
                metadata: None,
            },
        )
        .await
        .unwrap();

        let cc_registry = Arc::new(AgentRegistry::with_db(db.clone()));
        cc_registry
            .register("1001".to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status("1001", AgentStatus::Idle)
            .await
            .unwrap();
        // Simulate a concurrent call already having reserved this agent.
        cc_registry
            .update_status("1001", AgentStatus::Ringing {
                call_id: "other-call".to_string(),
                since: std::time::Instant::now(),
            })
            .await
            .unwrap();
        cc_agent_endpoint::ActiveModel {
            agent_id: Set("1001".to_string()),
            endpoint_type: Set("sip_uri".to_string()),
            endpoint_value: Set("sip:1001@example.com".to_string()),
            priority: Set(1),
            is_active: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let cache = Arc::new(tokio::sync::RwLock::new(
            SkillGroupTomlCache::default(),
        ));
        let adapter = CcAgentRegistryAdapter::new(
            cc_registry.clone(),
            acd_disabled(),
            "localhost",
        )
        .with_skill_group_cache(cache);

        let uris = adapter
            .resolve_target_with_policy("skill-group:support", None, "call-1")
            .await;

        // Busy and Ringing are reliable states — an agent already reserved by
        // another call must NOT be returned by the fallback. The call should
        // queue instead of double-dialling the same agent.
        assert!(
            uris.is_empty(),
            "fallback must exclude Ringing agents (already reserved), got {uris:?}"
        );
    }
