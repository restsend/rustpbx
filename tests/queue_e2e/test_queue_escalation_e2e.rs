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
    use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
    use rustpbx::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;
    use tokio::time::sleep;

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
            overflow_mode: None,
            overflow_after_secs: None,
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
        let _ = tracing_subscriber::fmt().try_init();

        let port = portpicker::pick_unused_port().unwrap_or(15070);

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
        for (id, skills) in [
            ("agent1", vec!["support"]),
            ("agent2", vec!["support_l2"]),
            ("agent3", vec!["support_l2"]),
        ] {
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
        // Short server-side ring timeout: the primary offer is deliberately
        // unanswered, and since the ringing-gate fix the widening waits for
        // the in-flight offer to end before it dials the overflow agents.
        cc_registry.set_desk_behavior(rustpbx::addons::cc::config::DeskBehaviorConfig {
            ring_no_answer_secs: 3,
            ..Default::default()
        });

        let cc_registry_reset = cc_registry.clone();
        let (sg_tx, mut sg_rx) =
            tokio::sync::mpsc::unbounded_channel::<
                rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent,
            >();
        let adapter = Arc::new(
            CcAgentRegistryAdapter::new(
                cc_registry,
                Arc::new(AcdEngine::new(AcdConfig::default())),
                "localhost",
            )
            .with_skill_group_cache(skill_group_cache)
            .with_skill_group_event_tx(sg_tx),
        );

        // ── RWI webhook capture (skill_group_* + queue_* contract) ────────
        let capture = crate::common::webhook_capture::WebhookCapture::start().await;
        let gateway: rustpbx::rwi::RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
            let mut gw = rustpbx::rwi::RwiGateway::new();
            gw.set_webhook_tx(rustpbx::rwi::webhook::start_rwi_webhook_handler(
                rustpbx::config::LocatorWebhookConfig {
                    url: capture.url.clone(),
                    events: [
                        "call_created",
                        "call_ringing",
                        "call_answered",
                        "call_hangup",
                        "queue_joined",
                        "queue_agent_offered",
                        "queue_agent_no_answer",
                        "queue_agent_rejected",
                        "queue_agent_connected",
                        "queue_left",
                        "queue_overflow_joined",
                        "skill_group_call_joined",
                        "skill_group_call_queued",
                        "skill_group_agent_assigned",
                        "skill_group_candidates_found",
                        "skill_group_no_agent",
                        "skill_group_agent_no_answer",
                        "skill_group_agent_rejected",
                        "skill_group_position_changed",
                        "skill_group_agent_connected",
                        "skill_group_call_left",
                    ]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                    headers: None,
                    timeout_ms: Some(5000),
                    retries: None,
                    track_queue_latency: None,
                },
                rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
            ));
            gw
        }));
        {
            let gw = gateway.clone();
            tokio::spawn(async move {
                while let Some(event) = sg_rx.recv().await {
                    if let Some(rwi) =
                        rustpbx::addons::cc::translate_skill_group_event(event)
                    {
                        gw.read().broadcast_event(&rwi);
                    }
                }
            });
        }

        let connected: Arc<Mutex<Vec<CallSessionContext>>> = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<dyn CallSessionHook> = Arc::new(RecordingHook {
            connected: connected.clone(),
        });

        let server = crate::common::e2e_test_server::E2eTestServer::start_with_inject(
            escalation_proxy_config(port),
            crate::common::e2e_test_server::E2eTestServerInject {
            queue_enricher: None,
                users: ["caller", "agent1", "agent2", "agent3"]
                    .into_iter()
                    .enumerate()
                    .map(|(idx, username)| SipUser {
                        id: (idx + 1) as u64,
                        username: username.to_string(),
                        password: Some("password".to_string()),
                        enabled: true,
                        realm: Some("127.0.0.1".to_string()),
                        ..Default::default()
                    })
                    .collect(),
                session_hook: Some(hook),
                agent_registry: Some(adapter),
                rwi_gateway: Some(gateway),
                #[cfg(feature = "addon-cc")]
                cc_policy_db: None,
            },
        )
        .await
        .unwrap();

        let proxy_addr = server.proxy_addr;

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

        // agent3: second overflow-group agent — only dialed when the fair
        // round-robin head rotates to it on the SECOND escalated call.
        let mut agent3 = mk_ua("agent3", 26013);
        agent3.start().await.unwrap();
        agent3.register().await.unwrap();

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

        // ── Phase 2 (fair widening, single-call): the 2s threshold fires
        // while agent1's offer is in flight — the ringing gate defers the
        // widening until agent1's ring timeout (3s) ends it; the re-armed
        // timer resolves the union (agent1 excluded via the no-answer
        // rotation) and offers EXACTLY ONE overflow agent. Single-call
        // invariant: never a group ring. ─────────────────────────────────
        let mut invite2: Option<(Duration, TestUaEvent)> = None;
        let mut invite3: Option<(Duration, TestUaEvent)> = None;
        let widen_deadline = Instant::now() + Duration::from_secs(15);
        while invite2.is_none() && invite3.is_none() && Instant::now() < widen_deadline {
            if invite2.is_none() {
                invite2 = wait_for_event(
                    &mut agent2,
                    t0,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
            if invite3.is_none() {
                invite3 = wait_for_event(
                    &mut agent3,
                    t0,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
        }
        assert!(
            invite2.as_ref().xor(invite3.as_ref()).is_some(),
            "single-call widening must offer EXACTLY ONE overflow agent (2:{invite2:?} 3:{invite3:?})"
        );
        let widened_at = invite2
            .as_ref()
            .map(|(d, _)| *d)
            .or_else(|| invite3.as_ref().map(|(d, _)| *d))
            .unwrap();
        assert!(
            widened_at >= Duration::from_secs(3),
            "widening must be deferred past the ringing window (3s), fired at {widened_at:?}"
        );
        assert!(
            widened_at < Duration::from_secs(10),
            "widening must happen promptly after the offer ends, took {widened_at:?}"
        );

        // The winner answers → the call connects through the queue.
        let sdp_answer_for = |owner: &str, port: u32| {
            format!(
                "v=0\r\n\
                o={owner} 2 0 IN IP4 127.0.0.1\r\ns={owner}\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                m=audio {port} RTP/AVP 0 101\r\n\
                a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            )
        };
        if invite2.is_some() {
            let (_, TestUaEvent::IncomingCall(agent2_dialog, _)) = invite2.unwrap() else {
                unreachable!("matcher guarantees IncomingCall");
            };
            agent2
                .answer_call(&agent2_dialog, Some(sdp_answer_for("agent2", 30002)))
                .await
                .unwrap();
            wait_for_event(
                &mut agent2,
                t0,
                Duration::from_secs(5),
                |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent2_dialog),
            )
            .await
            .expect("agent2 leg must be established after answering");
        } else {
            let (_, TestUaEvent::IncomingCall(agent3_dialog, _)) = invite3.unwrap() else {
                unreachable!("matcher guarantees IncomingCall");
            };
            agent3
                .answer_call(&agent3_dialog, Some(sdp_answer_for("agent3", 30014)))
                .await
                .unwrap();
            wait_for_event(
                &mut agent3,
                t0,
                Duration::from_secs(5),
                |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent3_dialog),
            )
            .await
            .expect("agent3 leg must be established after answering");
        }

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
        wait_for_event(&mut agent1, t0, Duration::from_secs(5), |e| {
            matches!(e, TestUaEvent::CallTerminated(_))
        })
        .await
        .expect(
            "agent1's superseded escalation leg must be terminated after \
             first-answer-wins",
        );

        let _ = hangup_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
        sleep(Duration::from_millis(300)).await;

        // ── Second escalated call: the fair round-robin head must ROTATE ──
        // The adapter-owned rr counter persists across calls (ec720bc), so
        // the head of the second escalation union differs from the first:
        // call 1 dialled agent1 (primary head) → the widen replaced the legs
        // with the fair union tail (agent3, agent2). Reset EVERY agent the
        // widen may have left in Ringing — missing one (e.g. agent3, whose
        // leg was cancelled mid-ring when agent2 answered) leaves it
        // un-candidate-able for call 2 and the widening dials a shorter
        // union than the rotation contract guarantees.
        for id in ["agent1", "agent2", "agent3"] {
            // idle→idle is rejected by the state machine; only reset the
            // agents actually left in Ringing/Busy by call 1.
            let needs_reset = cc_registry_reset
                .get_agent(id)
                .await
                .map(|a| !matches!(a.status, AgentStatus::Idle))
                .unwrap_or(false);
            if needs_reset {
                cc_registry_reset
                    .update_status(id, AgentStatus::Idle)
                    .await
                    .unwrap();
            }
        }
        let connected_before = connected.lock().await.len();

        // Drain stale UA events from call 1 (e.g. agent3's cancelled widen
        // leg and agent1's superseded-leg teardown) so call-2 matchers only
        // see fresh signalling.
        sleep(Duration::from_millis(400)).await;
        for ua in [&mut agent1, &mut agent2, &mut agent3] {
            loop {
                match wait_for_event(ua, Instant::now(), Duration::from_millis(200), |_| true).await
                {
                    Some(_) => continue,
                    None => break,
                }
            }
        }

        let mut caller2 = mk_ua("caller", 26014);
        caller2.start().await.unwrap();

        let sdp_offer2 = "v=0\r\n\
            o=caller2 3 0 IN IP4 127.0.0.1\r\ns=caller2\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
            m=audio 30003 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();

        let t1 = Instant::now();
        let (hangup2_tx, hangup2_rx) = tokio::sync::oneshot::channel::<()>();
        let call_task2 = tokio::spawn(async move {
            let dialog_id = caller2.make_call("support", Some(sdp_offer2)).await?;
            let _ = hangup2_rx.await;
            caller2.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        });

        // Phase 1+2 (single-call widening, deferred gate): the entry dial is
        // agent1 (the only primary candidate); the 2s threshold fires while
        // agent1's offer is in flight — deferred until agent1's ring timeout
        // (3s) ends it; the widen then (agent1 excluded via the no-answer
        // rotation) offers EXACTLY ONE of {agent2, agent3} — never a group
        // ring, never the just-failed agent1. The old "agent2 head first"
        // expectation was an artifact of ring-all widening.
        let mut w2: Option<(Duration, TestUaEvent)> = None;
        let mut w3: Option<(Duration, TestUaEvent)> = None;
        let widen2_deadline = t1 + Duration::from_secs(15);
        while w2.is_none() && w3.is_none() && Instant::now() < widen2_deadline {
            if w2.is_none() {
                w2 = wait_for_event(
                    &mut agent2,
                    t1,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
            if w3.is_none() {
                w3 = wait_for_event(
                    &mut agent3,
                    t1,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
        }
        assert!(
            w2.as_ref().xor(w3.as_ref()).is_some(),
            "single-call widening of call 2 must offer EXACTLY ONE candidate (2:{w2:?} 3:{w3:?})"
        );
        let widened2_at = w2
            .as_ref()
            .map(|(d, _)| *d)
            .or_else(|| w3.as_ref().map(|(d, _)| *d))
            .unwrap();
        assert!(
            widened2_at >= Duration::from_secs(3),
            "widening must be deferred past the ringing window (3s), fired at {widened2_at:?}"
        );
        let sdp_answer_for2 = |owner: &str, port: u32| {
            format!(
                "v=0\r\n\
                o={owner} 4 0 IN IP4 127.0.0.1\r\ns={owner}\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                m=audio {port} RTP/AVP 0 101\r\n\
                a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            )
        };
        let winner_is_agent2 = w2.is_some();
        if winner_is_agent2 {
            let (_, TestUaEvent::IncomingCall(agent2_dialog, _)) = w2.unwrap() else {
                unreachable!("matcher guarantees IncomingCall");
            };
            agent2
                .answer_call(&agent2_dialog, Some(sdp_answer_for2("agent2", 30012)))
                .await
                .unwrap();
            wait_for_event(
                &mut agent2,
                t1,
                Duration::from_secs(5),
                |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent2_dialog),
            )
            .await
            .expect("agent2 leg must be established after answering");
        } else {
            let (_, TestUaEvent::IncomingCall(agent3_dialog, _)) = w3.unwrap() else {
                unreachable!("matcher guarantees IncomingCall");
            };
            agent3
                .answer_call(&agent3_dialog, Some(sdp_answer_for2("agent3", 30004)))
                .await
                .unwrap();
            wait_for_event(
                &mut agent3,
                t1,
                Duration::from_secs(5),
                |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent3_dialog),
            )
            .await
            .expect("agent3 leg must be established after answering");
        }

        // Single-call invariant: the overflow agent that was NOT offered
        // must never receive an INVITE.
        if winner_is_agent2 {
            let events = agent3.process_dialog_events().await.unwrap_or_default();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, TestUaEvent::IncomingCall(_, _))),
                "agent3 must never be dialed in call 2's widening (single-call)"
            );
        } else {
            let events = agent2.process_dialog_events().await.unwrap_or_default();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, TestUaEvent::IncomingCall(_, _))),
                "agent2 must never be dialed in call 2's widening (single-call)"
            );
        }

        // The queue connected call 2 as well (hook fired again).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if connected.lock().await.len() > connected_before {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "on_call_connected must fire for call 2"
            );
            sleep(Duration::from_millis(50)).await;
        }

        let _ = hangup2_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task2).await;
        sleep(Duration::from_millis(300)).await;

        // ── RWI contract for the overflow chain (call 2: support → l2) ────
        // The stage switch must be visible in BOTH families: the core
        // `queue_overflow_joined` plus the full-fidelity join signal, and
        // the terminal carries the complete picture.
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let ready = {
                    let events = capture.received.lock().unwrap();
                    events
                        .iter()
                        .any(|v| v["event_type"].as_str() == Some("skill_group_call_left"))
                        && events
                            .iter()
                            .any(|v| v["event_type"].as_str() == Some("queue_overflow_joined"))
                };
                if ready || Instant::now() > deadline {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
            let events = capture.received.lock().unwrap();
            let mut call_ids: Vec<String> = events
                .iter()
                .filter_map(|v| v["event_type"].as_str().map(|_| v["call_id"].as_str()))
                .filter_map(|c| c.map(String::from))
                .collect();
            call_ids.dedup();
            let overflow_call = call_ids
                .last()
                .cloned()
                .expect("overflow call captured");
            let evs: Vec<serde_json::Value> = events
                .iter()
                .filter(|v| v["call_id"].as_str() == Some(overflow_call.as_str()))
                .cloned()
                .collect();
            let types: Vec<&str> = evs
                .iter()
                .filter_map(|v| v["event_type"].as_str())
                .collect();
            assert!(
                types.contains(&"queue_overflow_joined"),
                "stage switch must emit queue_overflow_joined: {types:?}"
            );
            assert!(
                types.contains(&"skill_group_call_joined"),
                "join signal present: {types:?}"
            );
            // The escalation round re-announces an assignment (attempt
            // increments) and the terminal pair closes the story.
            let assigned_count = types
                .iter()
                .filter(|t| **t == "skill_group_agent_assigned")
                .count();
            assert!(
                assigned_count >= 1,
                "assignment events present: {types:?}"
            );
            assert!(
                types.contains(&"queue_agent_connected"),
                "connect present: {types:?}"
            );
            assert!(
                types.contains(&"skill_group_agent_connected")
                    && types.contains(&"skill_group_call_left"),
                "skill_group terminal pair present: {types:?}"
            );
            let left = evs
                .iter()
                .find(|v| v["event_type"].as_str() == Some("skill_group_call_left"))
                .expect("call_left envelope");
            assert_eq!(
                left["event"]["reason"].as_str(),
                Some("connected"),
                "terminal reason: {left}"
            );

            println!("── overflow RWI timeline (call {overflow_call}) ──");
            for (i, ev) in evs.iter().enumerate() {
                let t = types[i];
                if t.starts_with("queue_") || t.starts_with("skill_group_") {
                    println!(
                        "  {:2} {:36} agent={:?} attempt={:?} reason={:?}",
                        i,
                        t,
                        ev["event"]["agent_id"].as_str(),
                        ev["event"]["attempt"].as_u64(),
                        ev["event"]["reason"].as_str(),
                    );
                }
            }
            println!("── end timeline ──");
        }

        server.stop();
    }

    /// The overflow rule book, end to end on real SIP (SEQUENTIAL mode,
    /// default idle-only scheduling):
    /// 1. ringing gate — the widen waits for agent1's offer to end (3s ring
    ///    timeout), it never races a live offer;
    /// 2. single-call — the widen offers EXACTLY ONE overflow agent;
    /// 3. pool re-offer — when that offer times out, the wait-retention poll
    ///    (covering primary + triggered overflow groups) offers the NEXT
    ///    pool candidate;
    /// 4. no-answer rotation — the just-failed agents are never re-offered
    ///    before fresh candidates;
    /// 5. break exclusion — an Away (小休) agent is NEVER dialed (default
    ///    policy `available_states = [idle]`).
    #[tokio::test]
    async fn test_overflow_pool_rules_single_call_never_break_never_group_ring_e2e() {
        let _ = tracing_subscriber::fmt().try_init();

        let port = portpicker::pick_unused_port().unwrap_or(15075);

        // Sequential overflow: per-stage dwell 2s (overflow_after_secs), the
        // queue's own max wait is 30s — decoupled so the pool re-offer can
        // actually happen after the first offer's ring timeout (with
        // max_wait == threshold the queue would fall back right after the
        // first widened offer ends).
        let mut cache = SkillGroupTomlCache::default();
        cache.groups.insert(
            "support".to_string(),
            SkillGroupConfigEntry {
                skill_group_id: "support".to_string(),
                display_name: None,
                skills_required: vec!["support".to_string()],
                overflow_groups: vec!["support_l2".to_string()],
                sla_target_secs: 30,
                max_wait_secs: 30,
                acd_policy: None,
                overflow_mode: Some("sequential".to_string()),
                overflow_after_secs: Some(2),
            },
        );
        cache.groups.insert(
            "support_l2".to_string(),
            group("support_l2", &["support_l2"], &[], 90),
        );
        let skill_group_cache = Arc::new(tokio::sync::RwLock::new(cache));

        let cc_registry = Arc::new(AgentRegistry::new());
        for (id, skills) in [
            ("agent1", vec!["support"]),
            ("agent2", vec!["support_l2"]),
            ("agent3", vec!["support_l2"]),
            ("agent_rest", vec!["support_l2"]),
        ] {
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
        // agent_rest is on BREAK (小休): default policy schedules idle only —
        // it must never be dialed, not by the widen, not by re-polls.
        cc_registry
            .update_status("agent_rest", AgentStatus::Away(String::new()))
            .await
            .unwrap();
        // Deterministic ring timeout for the unanswered offers.
        cc_registry.set_desk_behavior(rustpbx::addons::cc::config::DeskBehaviorConfig {
            ring_no_answer_secs: 3,
            ..Default::default()
        });

        let adapter = Arc::new(
            CcAgentRegistryAdapter::new(
                cc_registry.clone(),
                Arc::new(AcdEngine::new(AcdConfig::default())),
                "localhost",
            )
            .with_skill_group_cache(skill_group_cache),
        );

        let connected: Arc<Mutex<Vec<CallSessionContext>>> = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<dyn CallSessionHook> = Arc::new(RecordingHook {
            connected: connected.clone(),
        });

        let server = crate::common::e2e_test_server::E2eTestServer::start_with_inject(
            escalation_proxy_config(port),
            crate::common::e2e_test_server::E2eTestServerInject {
                queue_enricher: None,
                users: ["caller", "agent1", "agent2", "agent3", "agent_rest"]
                    .into_iter()
                    .enumerate()
                    .map(|(idx, username)| SipUser {
                        id: (idx + 1) as u64,
                        username: username.to_string(),
                        password: Some("password".to_string()),
                        enabled: true,
                        realm: Some("127.0.0.1".to_string()),
                        ..Default::default()
                    })
                    .collect(),
                session_hook: Some(hook),
                agent_registry: Some(adapter),
                rwi_gateway: None,
                #[cfg(feature = "addon-cc")]
                cc_policy_db: None,
            },
        )
        .await
        .unwrap();
        let proxy_addr = server.proxy_addr;

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

        let mut agent1 = mk_ua("agent1", 26110);
        agent1.start().await.unwrap();
        agent1.register().await.unwrap();
        let mut agent2 = mk_ua("agent2", 26111);
        agent2.start().await.unwrap();
        agent2.register().await.unwrap();
        let mut agent3 = mk_ua("agent3", 26112);
        agent3.start().await.unwrap();
        agent3.register().await.unwrap();
        let mut agent_rest = mk_ua("agent_rest", 26113);
        agent_rest.start().await.unwrap();
        agent_rest.register().await.unwrap();
        let mut caller = mk_ua("caller", 26114);
        caller.start().await.unwrap();

        let sdp_offer = "v=0\r\n\
            o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
            m=audio 30020 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();

        let t0 = Instant::now();
        let (hangup_tx, hangup_rx) = tokio::sync::oneshot::channel::<()>();
        let call_task = tokio::spawn(async move {
            let dialog_id = caller.make_call("support", Some(sdp_offer)).await?;
            let _ = hangup_rx.await;
            caller.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        });

        // ── Rule 1 setup: agent1 rings and never answers ─────────────────
        wait_for_event(&mut agent1, t0, Duration::from_secs(8), |e| {
            matches!(e, TestUaEvent::IncomingCall(_, _))
        })
        .await
        .expect("agent1 (primary) must receive the INVITE first");

        // ── Rule 1+2: deferred widening offers EXACTLY ONE overflow agent ─
        let mut w2: Option<(Duration, TestUaEvent)> = None;
        let mut w3: Option<(Duration, TestUaEvent)> = None;
        let widen_deadline = t0 + Duration::from_secs(15);
        while w2.is_none() && w3.is_none() && Instant::now() < widen_deadline {
            if w2.is_none() {
                w2 = wait_for_event(
                    &mut agent2,
                    t0,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
            if w3.is_none() {
                w3 = wait_for_event(
                    &mut agent3,
                    t0,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
        }
        assert!(
            w2.as_ref().xor(w3.as_ref()).is_some(),
            "the widen must offer EXACTLY ONE overflow agent (2:{w2:?} 3:{w3:?})"
        );
        let widened_at = w2
            .as_ref()
            .map(|(d, _)| *d)
            .or_else(|| w3.as_ref().map(|(d, _)| *d))
            .unwrap();
        assert!(
            widened_at >= Duration::from_secs(3),
            "widening must be deferred past agent1's ringing window (3s), fired at {widened_at:?}"
        );
        let first_winner_is_agent2 = w2.is_some();

        // ── Rule 3 setup: the first offer is left unanswered too ─────────
        let (first_winner_terminated_at, _) = if first_winner_is_agent2 {
            wait_for_event(&mut agent2, t0, Duration::from_secs(8), |e| {
                matches!(e, TestUaEvent::CallTerminated(_))
            })
            .await
            .expect("the first widened offer must end at its ring timeout")
        } else {
            wait_for_event(&mut agent3, t0, Duration::from_secs(8), |e| {
                matches!(e, TestUaEvent::CallTerminated(_))
            })
            .await
            .expect("the first widened offer must end at its ring timeout")
        };
        let _ = first_winner_terminated_at;

        // ── Rule 3+4: the re-poll offers the OTHER overflow candidate ────
        let mut r2: Option<(Duration, TestUaEvent)> = None;
        let mut r3: Option<(Duration, TestUaEvent)> = None;
        let reoffer_deadline = Instant::now() + Duration::from_secs(15);
        while r2.is_none() && r3.is_none() && Instant::now() < reoffer_deadline {
            if first_winner_is_agent2 && r3.is_none() {
                r3 = wait_for_event(
                    &mut agent3,
                    t0,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
            if !first_winner_is_agent2 && r2.is_none() {
                r2 = wait_for_event(
                    &mut agent2,
                    t0,
                    Duration::from_millis(250),
                    |e| matches!(e, TestUaEvent::IncomingCall(_, _)),
                )
                .await;
            }
        }
        assert!(
            r2.as_ref().xor(r3.as_ref()).is_some(),
            "the pool re-offer must go to the OTHER overflow candidate (2:{r2:?} 3:{r3:?})"
        );
        let sdp_answer_for = |owner: &str, port: u32| {
            format!(
                "v=0\r\n\
                o={owner} 2 0 IN IP4 127.0.0.1\r\ns={owner}\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                m=audio {port} RTP/AVP 0 101\r\n\
                a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            )
        };
        if r2.is_some() {
            let (_, TestUaEvent::IncomingCall(agent2_dialog, _)) = r2.unwrap() else {
                unreachable!("matcher guarantees IncomingCall");
            };
            agent2
                .answer_call(&agent2_dialog, Some(sdp_answer_for("agent2", 30022)))
                .await
                .unwrap();
            wait_for_event(
                &mut agent2,
                t0,
                Duration::from_secs(5),
                |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent2_dialog),
            )
            .await
            .expect("agent2 leg must be established");
        } else {
            let (_, TestUaEvent::IncomingCall(agent3_dialog, _)) = r3.unwrap() else {
                unreachable!("matcher guarantees IncomingCall");
            };
            agent3
                .answer_call(&agent3_dialog, Some(sdp_answer_for("agent3", 30023)))
                .await
                .unwrap();
            wait_for_event(
                &mut agent3,
                t0,
                Duration::from_secs(5),
                |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == agent3_dialog),
            )
            .await
            .expect("agent3 leg must be established");
        }

        // The queue's on_call_connected hook fired for the session.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !connected.lock().await.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "on_call_connected must fire once the pool re-offer answers"
            );
            sleep(Duration::from_millis(50)).await;
        }

        // ── Rule 5 + final invariants: drain every UA ────────────────────
        // agent_rest (小休): ZERO invites across the whole call.
        let rest_events = agent_rest.process_dialog_events().await.unwrap_or_default();
        assert!(
            rest_events.is_empty(),
            "a break (Away) agent must NEVER be dialed, got: {rest_events:?}"
        );
        // agent1: entry INVITE only — the widen/re-poll never re-dialed the
        // just-failed primary agent. (The entry INVITE was consumed by the
        // phase-1 wait above — the drain must hold NO further IncomingCall.)
        let agent1_events = agent1.process_dialog_events().await.unwrap_or_default();
        assert!(
            !agent1_events
                .iter()
                .any(|e| matches!(e, TestUaEvent::IncomingCall(_, _))),
            "agent1 must never be re-dialed after its ring terminal, got: {agent1_events:?}"
        );

        let _ = hangup_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
        sleep(Duration::from_millis(300)).await;

        server.stop();
    }
}
