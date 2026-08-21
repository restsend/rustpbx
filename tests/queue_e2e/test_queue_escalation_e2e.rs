//! Queue overflow escalation E2E (real SIP, CC addon wired).
//!
//! Exercises the full "priority group first → wait timeout → fair widening"
//! pipeline through every production layer:
//!
//!   caller INVITE → route queue (target `skill-group:support`)
//!     → SipSession::start_queue_app (captures the primary skill group)
//!     → resolve_custom_targets → CC adapter (primary group agents only)
//!     → QueueApp dials the PRIMARY group agent (agent1, never answers)
//!     → auto-armed `escalation_check` timer fires at the group's
//!       `max_wait_secs` threshold
//!     → resolve_escalation_targets: union of primary + `overflow_groups`,
//!       fair (round-robin) ordered, primary reserved atomically
//!     → cumulative escalation dials the widened agent (agent2)
//!     → agent2 answers → connected, agent1's leg cancelled.
//!
//! The escalation timeline here is synthesized from the skill group's own
//! `overflow_groups = ["support_l2"]` + `max_wait_secs = 2` (no ACD policy
//! configured) — the simplest configuration surface of the feature.

#[cfg(feature = "addon-cc")]
mod escalation_e2e {
    use rustpbx::addons::cc::SkillGroupConfigEntry;
    use rustpbx::addons::cc::SkillGroupTomlCache;
    use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
    use rustpbx::addons::cc::agent::{AgentRegistry, AgentStatus};
    use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
    use rustpbx::call::user::SipUser;
    use rustpbx::config::ProxyConfig;
    use rustpbx::proxy::locator::MemoryLocator;
    use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
    use rustpbx::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    use rustpbx::proxy::server::SipServerBuilder;
    use rustpbx::proxy::user::MemoryUserBackend;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;
    use tokio::time::sleep;
    use tokio_util::sync::CancellationToken;

    use crate::common::test_helpers::register_standard_modules;
    use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};

    fn group(
        id: &str,
        skills: &[&str],
        overflow_groups: &[&str],
        max_wait_secs: i32,
    ) -> SkillGroupConfigEntry {
        SkillGroupConfigEntry {
            skill_group_id: id.to_string(),
            display_name: None,
            skills_required: skills.iter().map(|s| s.to_string()).collect(),
            overflow_groups: overflow_groups.iter().map(|s| s.to_string()).collect(),
            sla_target_secs: 30,
            max_wait_secs,
            acd_policy: None,
        }
    }

    fn escalation_proxy_config(port: u16) -> ProxyConfig {
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

        // Queue "support" dials the skill group `support` — the primary
        // group whose `overflow_groups` defines the widening.
        let queue_config = RouteQueueConfig {
            name: Some("support".to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "skill-group:support".to_string(),
                    label: Some("Support".to_string()),
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
    struct RecordingHook {
        connected: Arc<Mutex<Vec<CallSessionContext>>>,
    }

    #[async_trait::async_trait]
    impl CallSessionHook for RecordingHook {
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

    /// Drive a TestUa until a matching event arrives; returns the event
    /// together with its arrival time relative to `since`. The returned
    /// event is a clone — it is also consumed from the UA's queue.
    async fn wait_for_event<F>(
        ua: &mut TestUa,
        since: Instant,
        timeout: Duration,
        matcher: F,
    ) -> Option<(Duration, TestUaEvent)>
    where
        F: Fn(&TestUaEvent) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let events = match tokio::time::timeout(remaining, ua.process_dialog_events()).await {
                Ok(Ok(events)) => events,
                _ => return None,
            };
            for event in events {
                if matcher(&event) {
                    return Some((since.elapsed(), event));
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn test_queue_overflow_escalation_priority_then_fair_widening_e2e() {
        let _ = tracing_subscriber::fmt::try_init();

        let port = portpicker::pick_unused_port().unwrap_or(15070);
        let config = Arc::new(escalation_proxy_config(port));

        let user_backend = MemoryUserBackend::new(None);
        for u in [
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
                username: "agent1".to_string(),
                password: Some("password".to_string()),
                enabled: true,
                realm: Some("127.0.0.1".to_string()),
                ..Default::default()
            },
            SipUser {
                id: 3,
                username: "agent2".to_string(),
                password: Some("password".to_string()),
                enabled: true,
                realm: Some("127.0.0.1".to_string()),
                ..Default::default()
            },
        ] {
            user_backend.create_user(u).await.unwrap();
        }

        // ── CC addon wiring (the part the production proxy gets from the
        // addon initialization) ──────────────────────────────────────────
        // Primary group `support` overflows to `support_l2` after 2 queued
        // seconds, fairly.
        let mut cache = SkillGroupTomlCache::default();
        cache.groups.insert(
            "support".to_string(),
            group("support", &["support"], &["support_l2"], 2),
        );
        cache.groups.insert(
            "support_l2".to_string(),
            group("support_l2", &["support_l2"], &[], 90),
        );
        let skill_group_cache = Arc::new(tokio::sync::RwLock::new(cache));

        let cc_registry = Arc::new(AgentRegistry::new());
        for (id, skills) in [("agent1", vec!["support"]), ("agent2", vec!["support_l2"])] {
            cc_registry
                .register(
                    id.to_string(),
                    skills.into_iter().map(|s| s.to_string()).collect(),
                    1,
                )
                .await
                .unwrap();
            cc_registry
                .update_status(id, AgentStatus::Idle)
                .await
                .unwrap();
        }

        let adapter = Arc::new(
            CcAgentRegistryAdapter::new(
                cc_registry,
                Arc::new(AcdEngine::new(AcdConfig::default())),
                "localhost",
            )
            .with_skill_group_cache(skill_group_cache),
        );

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();
        let connected: Arc<Mutex<Vec<CallSessionContext>>> = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<dyn CallSessionHook> = Arc::new(RecordingHook {
            connected: connected.clone(),
        });

        let builder = register_standard_modules(
            SipServerBuilder::new(config)
                .with_user_backend(Box::new(user_backend))
                .with_locator(Box::new(locator))
                .with_cancel_token(cancel_token.clone())
                .with_session_hook(hook),
        )
        .with_agent_registry(adapter);

        let server = builder.build().await.unwrap();
        let proxy_addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        sleep(Duration::from_millis(150)).await;

        let mk_ua = |username: &'static str, port_hint: u16| {
            TestUa::new(TestUaConfig {
                webrtc: false,
                username: username.to_string(),
                password: "password".to_string(),
                realm: "127.0.0.1".to_string(),
                local_port: portpicker::pick_unused_port().unwrap_or(port_hint),
                proxy_addr,
            })
        };

        // agent1: primary-group agent — receives the INVITE but NEVER
        // answers (simulates the priority group being occupied).
        let mut agent1 = mk_ua("agent1", 26010);
        agent1.start().await.unwrap();
        agent1.register().await.unwrap();

        // agent2: overflow-group agent — answers once the widened dial
        // reaches it.
        let mut agent2 = mk_ua("agent2", 26011);
        agent2.start().await.unwrap();
        agent2.register().await.unwrap();

        let mut caller = mk_ua("caller", 26012);
        caller.start().await.unwrap();

        let sdp_offer = "v=0\r\n\
            o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
            m=audio 30001 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();

        let t0 = Instant::now();
        // The caller INVITE runs concurrently — the agents' dialogs must be
        // polled while the caller waits for its first response (the agents'
        // provisional replies are what satisfy do_invite).
        let (hangup_tx, hangup_rx) = tokio::sync::oneshot::channel::<()>();
        let call_task = tokio::spawn(async move {
            let dialog_id = caller.make_call("support", Some(sdp_offer)).await?;
            // Hold the call until the test body is done asserting.
            let _ = hangup_rx.await;
            caller.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        });

        // ── Phase 1 (priority): the primary-group agent is dialled first ──
        let (agent1_invite_at, _) = wait_for_event(&mut agent1, t0, Duration::from_secs(8), |e| {
            matches!(e, TestUaEvent::IncomingCall(_, _))
        })
        .await
        .expect("agent1 (primary group) must receive the INVITE first");
        assert!(
            agent1_invite_at < Duration::from_secs(2),
            "phase 1 must dial the primary group immediately, took {agent1_invite_at:?}"
        );
        // agent1 deliberately does NOT answer.

        // ── Phase 2 (fair widening): after the group's max_wait_secs (2s)
        // the auto-armed escalation timer fires, resolves the union
        // (support ∪ support_l2) and dials the overflow agent ─────────────
        let (agent2_invite_at, agent2_invite) =
            wait_for_event(&mut agent2, t0, Duration::from_secs(10), |e| {
                matches!(e, TestUaEvent::IncomingCall(_, _))
            })
            .await
            .expect("agent2 (overflow group) must receive the widened INVITE");
        assert!(
            agent2_invite_at >= Duration::from_secs(2),
            "widening must respect the max_wait_secs threshold (2s), fired at {agent2_invite_at:?}"
        );
        assert!(
            agent2_invite_at < Duration::from_secs(6),
            "widening must happen promptly after the threshold, took {agent2_invite_at:?}"
        );

        // agent2 answers → the call connects through the queue.
        let TestUaEvent::IncomingCall(agent2_dialog, _) = agent2_invite else {
            unreachable!("matcher guarantees IncomingCall");
        };
        let sdp_answer = "v=0\r\n\
            o=agent2 2 0 IN IP4 127.0.0.1\r\ns=agent2\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
            m=audio 30002 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();
        agent2
            .answer_call(&agent2_dialog, Some(sdp_answer))
            .await
            .unwrap();

        // Both sides see the established call.
        wait_for_event(
            &mut agent2,
            t0,
            Duration::from_secs(5),
            |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent2_dialog),
        )
        .await
        .expect("agent2 leg must be established after answering");

        // The queue's on_call_connected hook fired for the session.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !connected.lock().await.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "on_call_connected must fire once the overflow agent answers"
            );
            sleep(Duration::from_millis(50)).await;
        }

        // agent1's superseded leg is torn down (first-answer-wins removes
        // the remaining escalation legs).
        let _ = wait_for_event(&mut agent1, t0, Duration::from_secs(5), |e| {
            matches!(e, TestUaEvent::CallTerminated(_))
        })
        .await;

        let _ = hangup_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
        sleep(Duration::from_millis(300)).await;

        cancel_token.cancel();
        serve_task.abort();
    }
}
