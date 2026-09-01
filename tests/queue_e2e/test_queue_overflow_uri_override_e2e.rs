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
        }
    }

    /// Tree IVR: press 1 → transfer to the queue. With `with_overflow` the
    /// transfer target carries the per-call overflow params; without, it is
    /// a plain skill-group queue transfer (control scenario).
    fn ivr_toml(with_overflow: bool) -> String {
        let queue_target = if with_overflow {
            format!(
                "queue:{QUEUE_NAME}?target=skillgroup:{PRIMARY_GROUP}\
&overflow_group={OVERFLOW_GROUP}&overflow_after=2&overflow_mode=cumulative"
            )
        } else {
            format!("queue:{QUEUE_NAME}?target=skillgroup:{PRIMARY_GROUP}")
        };
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
    }

    /// Start the server with the CC adapter wired to both skill groups
    /// (both with EMPTY `overflow_groups` — the widening can only come
    /// from the URI) and the tree-IVR route.
    async fn start_harness(port: u16, with_overflow: bool) -> Harness {
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

        let adapter = Arc::new(
            CcAgentRegistryAdapter::new(
                cc_registry,
                Arc::new(AcdEngine::new(AcdConfig::default())),
                "localhost",
            )
            .with_skill_group_cache(skill_group_cache),
        );

        let hook = Arc::new(RecordingHook {
            connected: Arc::new(Mutex::new(Vec::new())),
        });
        let session_hook: Arc<dyn CallSessionHook> = hook.clone();

        let ivr_path = std::env::temp_dir().join(format!(
            "overflow-uri-ivr-{}{}.toml",
            port,
            if with_overflow { "-ovf" } else { "-plain" }
        ));
        std::fs::write(&ivr_path, ivr_toml(with_overflow)).unwrap();

        let server = crate::common::e2e_test_server::E2eTestServer::start_with_inject(
            proxy_config(port, &ivr_path),
            crate::common::e2e_test_server::E2eTestServerInject {
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
            },
        )
        .await
        .unwrap();

        Harness {
            server,
            hook,
            _ivr_path: ivr_path,
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

    const AGENT_SALES_SDP_PORT: u32 = 30101;
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
        let harness = start_harness(port, true).await;
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

        // ── Phase 2: URI `overflow_after=2` — widened INVITE reaches the
        // URI-specified overflow group ~2s in, while the primary leg keeps
        // ringing (cumulative) ────────────────────────────────────────────
        let (l2_invite_at, l2_invite) =
            wait_for_event(&mut agent_l2, t0, Duration::from_secs(10), |e| {
                matches!(e, TestUaEvent::IncomingCall(_, _))
            })
            .await
            .expect("agent_l2 must receive the widened INVITE from the URI overflow params");
        assert!(
            l2_invite_at >= Duration::from_secs(2),
            "widening must respect the URI overflow_after=2 threshold, fired at {l2_invite_at:?}"
        );
        assert!(
            l2_invite_at < Duration::from_secs(6),
            "widening must happen promptly after the URI threshold, took {l2_invite_at:?}"
        );
        // Cumulative: the primary leg is still ringing (not cancelled).
        assert!(
            !matches!(
                agent_sales.process_dialog_events().await,
                Ok(events) if events.iter().any(|e| matches!(e, TestUaEvent::CallTerminated(_)))
            ),
            "primary leg must keep ringing in cumulative mode before anybody answers"
        );

        // agent_l2 answers → connected.
        let TestUaEvent::IncomingCall(l2_dialog, offer) = l2_invite else {
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
        let harness = start_harness(port, false).await;
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
}
