//! Queue overflow URI override E2E (real SIP, CC addon wired).
//!
//! Proves that a per-call transfer URI can carry overflow parameters that
//! **override** the skill-group configuration:
//!
//! caller INVITE → route → tree IVR (press 1)
//!   → transfer `queue:sales_q?target=skillgroup:sales
//!        &overflow_group=support_l2&overflow_after=2&overflow_mode=cumulative`
//!   → handle_queue_transfer (parses the overflow query params)
//!   → start_queue_app → PendingQueuePlan.overflow_overrides
//!   → builtin app factory applies the URI overrides on top of the
//!     registry-synthesized plan (which is EMPTY here: the primary group
//!     has no `overflow_groups` and no ACD policy)
//!   → QueueApp escalates after 2s into `support_l2`.
//!
//! The primary group `sales` deliberately has `overflow_groups = []`, so the
//! widening can only come from the URI. The control scenario drops the
//! overflow params and asserts the overflow agent is never dialed.

#[cfg(feature = "addon-cc")]
mod overflow_uri_e2e {
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

    const IVR_NUMBER: &str = "9200";
    const QUEUE_NAME: &str = "sales_q";
    const PRIMARY_GROUP: &str = "sales";
    const OVERFLOW_GROUP: &str = "support_l2";

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

    /// Tree IVR: press 1 → transfer to the queue. `mode` selects the
    /// per-call `overflow_mode` URI param; the overflow group/threshold are
    /// only appended WITH a mode. None = plain skill-group queue transfer
    /// (the control scenario must carry NO overflow params at all).
    fn ivr_toml(mode: Option<&str>) -> String {
        let mut queue_target = format!("queue:{QUEUE_NAME}?target=skillgroup:{PRIMARY_GROUP}");
        if let Some(mode) = mode {
            queue_target.push_str(&format!(
                "&overflow_group={OVERFLOW_GROUP}&overflow_after=2&overflow_mode={mode}"
            ));
        }
        // The greeting file does not exist → tree IVR skips playback and
        // waits for DTMF immediately (no TTS / audio dependency in CI).
        format!(
            r#"
[ivr]
name = "overflow-ivr"

[ivr.root]
greeting = "sounds/definitely-missing-overflow-menu.wav"
timeout_ms = 10000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = {{ type = "transfer", target = "{queue_target}" }}
"#
        )
    }

    fn proxy_config(port: u16, ivr_file: &std::path::Path) -> ProxyConfig {
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
        config.ensure_user = Some(false);
        config.enable_latching = false;

        // Queue "sales_q" dials the primary skill group. The transfer URI's
        // `target=` override would replace this anyway — the queue config
        // only needs to exist so `handle_queue_transfer` can resolve it.
        let queue_config = RouteQueueConfig {
            name: Some(QUEUE_NAME.to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: format!("skill-group:{PRIMARY_GROUP}"),
                    label: Some("Sales".to_string()),
                }],
                ..Default::default()
            },
            accept_immediately: false,
            ..Default::default()
        };
        config.queues.insert(QUEUE_NAME.to_string(), queue_config);

        let route = RouteRule {
            name: "route_to_overflow_ivr".to_string(),
            priority: 10,
            match_conditions: MatchConditions {
                to_user: Some(IVR_NUMBER.to_string()),
                ..Default::default()
            },
            action: RouteAction {
                app: Some("ivr".to_string()),
                app_params: Some(serde_json::json!({
                    "file": ivr_file.to_string_lossy(),
                })),
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

    struct Harness {
        server: crate::common::e2e_test_server::E2eTestServer,
        hook: Arc<RecordingHook>,
        _ivr_path: std::path::PathBuf,
        /// Adapter skill-group lifecycle events (single-consumer drain).
        skill_events_rx: tokio::sync::mpsc::UnboundedReceiver<
            rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent,
        >,
    }

    /// Start the server with the CC adapter wired to both skill groups
    /// (both with EMPTY `overflow_groups` — the widening can only come
    /// from the URI) and the tree-IVR route.
    async fn start_harness(port: u16, mode: Option<&str>) -> Harness {
        let mut cache = SkillGroupTomlCache::default();
        cache.groups.insert(
            PRIMARY_GROUP.to_string(),
            group(PRIMARY_GROUP, &["sales"], &[], 60),
        );
        cache.groups.insert(
            OVERFLOW_GROUP.to_string(),
            group(OVERFLOW_GROUP, &["l2"], &[], 90),
        );
        let skill_group_cache = Arc::new(tokio::sync::RwLock::new(cache));

        let cc_registry = Arc::new(AgentRegistry::new());
        for (id, skills) in [("agent_sales", vec!["sales"]), ("agent_l2", vec!["l2"])] {
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
        // Short server-side ring timeout: the primary agent never answers, so
        // the offer must terminate quickly and deterministically — since the
        // ringing-gate fix, overflow waits for the in-flight offer to end.
        cc_registry.set_desk_behavior(rustpbx::addons::cc::config::DeskBehaviorConfig {
            ring_no_answer_secs: 3,
            ..Default::default()
        });

        let (sg_tx, sg_rx) =
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

        let hook = Arc::new(RecordingHook {
            connected: Arc::new(Mutex::new(Vec::new())),
        });
        let session_hook: Arc<dyn CallSessionHook> = hook.clone();

        let ivr_path = std::env::temp_dir().join(format!(
            "overflow-uri-ivr-{}{}.toml",
            port,
            match mode {
                None => "-plain".to_string(),
                Some(m) => format!("-{m}"),
            }
        ));
        std::fs::write(&ivr_path, ivr_toml(mode)).unwrap();

        let server = crate::common::e2e_test_server::E2eTestServer::start_with_inject(
            proxy_config(port, &ivr_path),
            crate::common::e2e_test_server::E2eTestServerInject {
            queue_enricher: None,
                users: ["caller", "agent_sales", "agent_l2"]
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
                session_hook: Some(session_hook),
                agent_registry: Some(adapter),
                rwi_gateway: None,
                #[cfg(feature = "addon-cc")]
                cc_policy_db: None,
            },
        )
        .await
        .unwrap();

        Harness {
            server,
            hook,
            _ivr_path: ivr_path,
            skill_events_rx: sg_rx,
        }
    }

    fn mk_ua(proxy_addr: std::net::SocketAddr, username: &'static str, port_hint: u16) -> TestUa {
        TestUa::new(TestUaConfig {
            webrtc: false,
            username: username.to_string(),
            password: "password".to_string(),
            realm: "127.0.0.1".to_string(),
            local_port: portpicker::pick_unused_port().unwrap_or(port_hint),
            proxy_addr,
        })
    }

    const AGENT_L2_SDP_PORT: u32 = 30102;

    fn answer_sdp(owner: &str, port: u32) -> String {
        format!(
            "v=0\r\n\
             o={owner} 2 0 IN IP4 127.0.0.1\r\ns={owner}\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio {port} RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
        )
    }

    /// URI overflow params must widen the queue to `support_l2` after
    /// `overflow_after=2` even though the primary group itself has NO
    /// overflow configuration (priority: URI > group config).
    #[tokio::test]
    async fn test_queue_overflow_uri_override_widens_to_uri_group_e2e() {
        let _ = tracing_subscriber::fmt().try_init();

        let port = portpicker::pick_unused_port().unwrap_or(15080);
        let harness = start_harness(port, Some("cumulative")).await;
        let proxy_addr = harness.server.proxy_addr;

        // agent_sales: primary-group agent — receives the INVITE but NEVER
        // answers (the escalation must come from the URI, not from his leg
        // failing).
        let mut agent_sales = mk_ua(proxy_addr, "agent_sales", 26020);
        agent_sales.start().await.unwrap();
        agent_sales.register().await.unwrap();

        // agent_l2: URI-specified overflow-group agent.
        let mut agent_l2 = mk_ua(proxy_addr, "agent_l2", 26021);
        agent_l2.start().await.unwrap();
        agent_l2.register().await.unwrap();

        let mut caller = mk_ua(proxy_addr, "caller", 26022);
        caller.start().await.unwrap();

        let sdp_offer = "v=0\r\n\
             o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio 30003 RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();

        let t0 = Instant::now();
        let (hangup_tx, hangup_rx) = tokio::sync::oneshot::channel::<()>();
        let call_task = tokio::spawn(async move {
            let dialog_id = caller.make_call(IVR_NUMBER, Some(sdp_offer)).await?;
            // IVR greeting is missing → DTMF is accepted immediately.
            tokio::time::sleep(Duration::from_millis(400)).await;
            caller.send_dtmf_info(&dialog_id, "1").await?;
            let _ = hangup_rx.await;
            caller.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        });

        // ── Phase 1: the primary group's agent is dialed first ────────────
        let (sales_invite_at, _) =
            wait_for_event(&mut agent_sales, t0, Duration::from_secs(8), |e| {
                matches!(e, TestUaEvent::IncomingCall(_, _))
            })
            .await
            .expect("agent_sales (primary group) must receive the INVITE first");
        assert!(
            sales_invite_at < Duration::from_secs(3),
            "primary agent must be dialed immediately, took {sales_invite_at:?}"
        );

        // ── Phase 2: ringing-gate — the 2s threshold fires while the offer is
        // in flight, so widening is DEFERRED until the primary leg's ring
        // timeout (3s) ends the offer; only then does the l2 INVITE go out ──
        let (sales_ended_at, _) =
            wait_for_event(&mut agent_sales, t0, Duration::from_secs(10), |e| {
                matches!(e, TestUaEvent::CallTerminated(_))
            })
            .await
            .expect("primary leg must terminate at its ring timeout");
        assert!(
            sales_ended_at >= Duration::from_secs(3),
            "the in-flight offer must run to its ring timeout, ended at {sales_ended_at:?}"
        );

        let (l2_invite_at, l2_invite) =
            wait_for_event(&mut agent_l2, t0, Duration::from_secs(10), |e| {
                matches!(e, TestUaEvent::IncomingCall(_, _))
            })
            .await
            .expect("agent_l2 must receive the widened INVITE after the offer ended");
        assert!(
            l2_invite_at > sales_ended_at,
            "widening must be deferred past the ringing window: l2 at {l2_invite_at:?} < sales end {sales_ended_at:?}"
        );
        assert!(
            l2_invite_at < sales_ended_at + Duration::from_secs(5),
            "widening must fire promptly after the offer ends, took {l2_invite_at:?}"
        );

        // agent_l2 answers → connected.
        let TestUaEvent::IncomingCall(l2_dialog, _offer) = l2_invite else {
            unreachable!("matcher guarantees IncomingCall");
        };
        agent_l2
            .answer_call(&l2_dialog, Some(answer_sdp("agent_l2", AGENT_L2_SDP_PORT)))
            .await
            .unwrap();
        wait_for_event(
            &mut agent_l2,
            t0,
            Duration::from_secs(5),
            |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == l2_dialog),
        )
        .await
        .expect("agent_l2 leg must be established after answering");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !harness.hook.connected.lock().await.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "on_call_connected must fire once the URI-overflow agent answers"
            );
            sleep(Duration::from_millis(50)).await;
        }

        let _ = hangup_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
        sleep(Duration::from_millis(300)).await;

        harness.server.stop();
    }

    /// Control: the same IVR → queue transfer WITHOUT overflow params. The
    /// group has no `overflow_groups` and no ACD policy, so no widening may
    /// ever happen — proving the previous scenario's widening came from the
    /// URI and not from the group configuration.
    #[tokio::test]
    async fn test_queue_without_uri_overflow_never_widens_e2e() {
        let _ = tracing_subscriber::fmt().try_init();

        let port = portpicker::pick_unused_port().unwrap_or(15090);
        let harness = start_harness(port, None).await;
        let proxy_addr = harness.server.proxy_addr;

        let mut agent_sales = mk_ua(proxy_addr, "agent_sales", 26030);
        agent_sales.start().await.unwrap();
        agent_sales.register().await.unwrap();

        let mut agent_l2 = mk_ua(proxy_addr, "agent_l2", 26031);
        agent_l2.start().await.unwrap();
        agent_l2.register().await.unwrap();

        let mut caller = mk_ua(proxy_addr, "caller", 26032);
        caller.start().await.unwrap();

        let sdp_offer = "v=0\r\n\
             o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio 30004 RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();

        let t0 = Instant::now();
        let (hangup_tx, hangup_rx) = tokio::sync::oneshot::channel::<()>();
        let call_task = tokio::spawn(async move {
            let dialog_id = caller.make_call(IVR_NUMBER, Some(sdp_offer)).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            caller.send_dtmf_info(&dialog_id, "1").await?;
            let _ = hangup_rx.await;
            caller.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        });

        // The primary agent is dialed (queue works normally).
        wait_for_event(&mut agent_sales, t0, Duration::from_secs(8), |e| {
            matches!(e, TestUaEvent::IncomingCall(_, _))
        })
        .await
        .expect("agent_sales must receive the INVITE");

        // …and well past the URI scenario's 2s threshold, NOTHING widens:
        // agent_l2 must stay silent for the whole observation window.
        let l2_event = wait_for_event(&mut agent_l2, t0, Duration::from_secs(6), |_| true).await;
        assert!(
            l2_event.is_none(),
            "agent_l2 must NEVER be dialed without URI overflow params, got {l2_event:?}"
        );

        let _ = hangup_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
        sleep(Duration::from_millis(300)).await;

        harness.server.stop();
    }

    const L2_SEQ_SDP_PORT: u32 = 30103;

    /// Sequential URI overflow (`overflow_mode=sequential&overflow_after=2`),
    /// with the ringing gate: at the 2s threshold the primary offer is still
    /// in flight, so the switch is DEFERRED until the primary leg's ring
    /// timeout ends it. Then the stage switch must (1) dial ONLY the URI
    /// overflow group's agent (never re-dial the primary), and (2) complete
    /// when the overflow agent answers — the "dequeue + re-enqueue"
    /// contract, driven purely by URI params.
    #[tokio::test]
    async fn test_queue_overflow_uri_sequential_switches_stage_e2e() {
        let _ = tracing_subscriber::fmt().try_init();

        let port = portpicker::pick_unused_port().unwrap_or(15100);
        let mut harness = start_harness(port, Some("sequential")).await;
        let proxy_addr = harness.server.proxy_addr;

        let mut agent_sales = mk_ua(proxy_addr, "agent_sales", 26040);
        agent_sales.start().await.unwrap();
        agent_sales.register().await.unwrap();

        let mut agent_l2 = mk_ua(proxy_addr, "agent_l2", 26041);
        agent_l2.start().await.unwrap();
        agent_l2.register().await.unwrap();

        let mut caller = mk_ua(proxy_addr, "caller", 26042);
        caller.start().await.unwrap();

        let sdp_offer = "v=0\r\n\
             o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio 30005 RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
            .to_string();

        let t0 = Instant::now();
        let (hangup_tx, hangup_rx) = tokio::sync::oneshot::channel::<()>();
        let call_task = tokio::spawn(async move {
            let dialog_id = caller.make_call(IVR_NUMBER, Some(sdp_offer)).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            caller.send_dtmf_info(&dialog_id, "1").await?;
            let _ = hangup_rx.await;
            caller.hangup(&dialog_id).await?;
            Ok::<_, anyhow::Error>(())
        });

        // ── Stage 1: primary agent dialed immediately, keeps ringing ──────
        wait_for_event(&mut agent_sales, t0, Duration::from_secs(8), |e| {
            matches!(e, TestUaEvent::IncomingCall(_, _))
        })
        .await
        .expect("agent_sales (stage 1) must receive the INVITE");

        // ── Stage switch DEFERRED by the ringing gate: at overflow_after=2
        // the offer is still in flight, so the switch waits for the ring
        // timeout (3s) to end it; the primary leg then terminates ─────────
        let (terminated_at, _) =
            wait_for_event(&mut agent_sales, t0, Duration::from_secs(10), |e| {
                matches!(e, TestUaEvent::CallTerminated(_))
            })
            .await
            .expect("the primary agent's ringing leg must terminate at its ring timeout");
        assert!(
            terminated_at >= Duration::from_secs(3),
            "the in-flight offer must run to its ring timeout (3s), ended at {terminated_at:?}"
        );

        // ── Stage 2: ONLY the overflow agent is dialed (after the switch) ──
        let (l2_invite_at, l2_invite) =
            wait_for_event(&mut agent_l2, t0, Duration::from_secs(8), |e| {
                matches!(e, TestUaEvent::IncomingCall(_, _))
            })
            .await
            .expect("agent_l2 (stage 2) must receive the INVITE after the switch");
        assert!(
            l2_invite_at > terminated_at,
            "stage 2 must dial only after the ringing window ended: l2 at {l2_invite_at:?} <= end {terminated_at:?}"
        );
        assert!(
            l2_invite_at < terminated_at + Duration::from_secs(5),
            "stage 2 must dial promptly after the switch, took {l2_invite_at:?}"
        );

        // The primary agent must NOT be re-dialed for the new stage.
        assert!(
            !matches!(
                agent_sales.process_dialog_events().await,
                Ok(events) if events.iter().any(|e| matches!(e, TestUaEvent::IncomingCall(_, _)))
            ),
            "sequential switch must never re-dial the departed stage's agent"
        );

        // Dispatcher-family switch marker: the adapter emitted
        // GroupSwitched (→ skill_group_call_moved{from, to, reason}) at the
        // stage switch — the skill_group family sees the transition, not
        // just the queue family.
        let mut saw_moved = false;
        while let Ok(ev) = harness.skill_events_rx.try_recv() {
            if matches!(
                ev,
                rustpbx::addons::cc::agent_registry_adapter::SkillGroupEvent::GroupSwitched {
                    ref from_group,
                    ref to_group,
                    ref reason,
                    ..
                } if from_group == PRIMARY_GROUP
                    && to_group == OVERFLOW_GROUP
                    && reason == "overflow"
            ) {
                saw_moved = true;
                break;
            }
        }
        assert!(
            saw_moved,
            "the stage switch must emit GroupSwitched (skill_group_call_moved) for {PRIMARY_GROUP} → {OVERFLOW_GROUP}"
        );

        // agent_l2 answers → the call completes on the overflow stage.
        let TestUaEvent::IncomingCall(l2_dialog, _offer) = l2_invite else {
            unreachable!("matcher guarantees IncomingCall");
        };
        agent_l2
            .answer_call(&l2_dialog, Some(answer_sdp("agent_l2_seq", L2_SEQ_SDP_PORT)))
            .await
            .unwrap();
        wait_for_event(
            &mut agent_l2,
            t0,
            Duration::from_secs(5),
            |e| matches!(e, TestUaEvent::CallEstablished(d) if *d == l2_dialog),
        )
        .await
        .expect("agent_l2 leg must be established after answering");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !harness.hook.connected.lock().await.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "on_call_connected must fire once the sequential overflow agent answers"
            );
            sleep(Duration::from_millis(50)).await;
        }

        let _ = hangup_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
        sleep(Duration::from_millis(300)).await;

        harness.server.stop();
    }
}
