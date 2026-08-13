use rustpbx::rwi::*;

    use rustpbx::call::DialDirection;
    use rustpbx::call::domain::{CallCommand, LegId};
    use rustpbx::call::runtime::ConferenceManager;
    use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
    use rustpbx::rwi::gateway::RwiGateway;
    use rustpbx::rwi::session::RwiCommandPayload;
    use parking_lot::RwLock;
    use std::sync::Arc;

    // caller_id normalization — the From-URI validity contract for originate.
    // A malformed From (e.g. a bare number -> user-less URI after a trunk host
    // rewrite) is rejected by carriers with "400 Invalid From".
    #[test]
    fn normalize_originate_caller_id_shapes() {
        use RwiCommandProcessor as P;
        let realm = "pbx.example.com";
        // Bare E.164 number: the common case — becomes sip:<num>@realm.
        assert_eq!(
            P::normalize_originate_caller_id(Some("+16142159851"), realm),
            "sip:+16142159851@pbx.example.com"
        );
        // Number without '+'.
        assert_eq!(
            P::normalize_originate_caller_id(Some("16142159851"), realm),
            "sip:16142159851@pbx.example.com"
        );
        // Full SIP URI with a user is preserved verbatim.
        assert_eq!(
            P::normalize_originate_caller_id(Some("sip:+18005550100@carrier.invalid"), realm),
            "sip:+18005550100@carrier.invalid"
        );
        // sips: URI with a user is preserved verbatim.
        assert_eq!(
            P::normalize_originate_caller_id(Some("sips:alice@secure.invalid"), realm),
            "sips:alice@secure.invalid"
        );
        // scheme present but NO user -> re-wrapped so From carries a user.
        assert_eq!(
            P::normalize_originate_caller_id(Some("sip:+16142159851"), realm),
            "sip:+16142159851@pbx.example.com"
        );
        // Bare user@host (no scheme) must NOT double-affix into
        // sip:user@host@realm — take the user token only.
        assert_eq!(
            P::normalize_originate_caller_id(Some("device@example.com"), realm),
            "sip:device@pbx.example.com"
        );
        // Params on a bare token don't leak into the URI host.
        assert_eq!(
            P::normalize_originate_caller_id(Some("+16142159851;user=phone"), realm),
            "sip:+16142159851@pbx.example.com"
        );
        // None / blank -> the unchanged rwi fallback.
        assert_eq!(
            P::normalize_originate_caller_id(None, realm),
            "sip:rwi@pbx.example.com"
        );
        assert_eq!(
            P::normalize_originate_caller_id(Some("  "), realm),
            "sip:rwi@pbx.example.com"
        );
        // Degenerate inputs that would extract an EMPTY user must fall back to
        // the safe default, never `sip:@realm`.
        for degenerate in ["sip:", "sips:", "@example.com", ";user=phone", "?x=1"] {
            assert_eq!(
                P::normalize_originate_caller_id(Some(degenerate), realm),
                "sip:rwi@pbx.example.com",
                "degenerate caller_id {degenerate:?} must fall back, not sip:@realm"
            );
        }
        // Every result must parse as a URI whose From would carry a user.
        for input in ["+16142159851", "sip:+16142159851", "device@example.com"] {
            let out = P::normalize_originate_caller_id(Some(input), realm);
            let uri = rsipstack::sip::Uri::try_from(out.as_str())
                .unwrap_or_else(|_| panic!("normalized caller_id must parse: {out}"));
            assert!(
                uri.auth.as_ref().is_some_and(|a| !a.user.is_empty()),
                "normalized From URI must carry a user; input={input} out={out}"
            );
        }
    }

    fn create_test_processor() -> (Arc<RwiCommandProcessor>, Arc<ConferenceManager>) {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway, cm.clone()));
        (processor, cm)
    }

    fn create_test_processor_with_registry(
        registry: Arc<ActiveProxyCallRegistry>,
    ) -> (Arc<RwiCommandProcessor>, Arc<ConferenceManager>) {
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(registry, gateway, cm.clone()));
        (processor, cm)
    }

    fn create_test_call(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> rustpbx::proxy::proxy_call::sip_session::SipSessionHandle {
        create_test_call_with_conference_manager(
            registry, session_id, caller, callee, direction, None,
        )
    }

    fn create_test_call_with_conference_manager(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
        _conference_manager: Option<Arc<ConferenceManager>>,
    ) -> rustpbx::proxy::proxy_call::sip_session::SipSessionHandle {
        use rustpbx::call::runtime::SessionId;
        use rustpbx::proxy::proxy_call::sip_session::SipSession;

        let id = SessionId::from(session_id);
        let (handle, mut cmd_rx) = SipSession::with_handle(id);

        rustpbx::utils::spawn(async move { while let Some(_cmd) = cmd_rx.recv().await {} });

        let entry = rustpbx::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: rustpbx::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        handle
    }

    fn create_test_call_with_rx(
        registry: &Arc<ActiveProxyCallRegistry>,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: DialDirection,
    ) -> (
        rustpbx::proxy::proxy_call::sip_session::SipSessionHandle,
        rustpbx::call::domain::CallCommandRx,
    ) {
        use rustpbx::call::runtime::SessionId;
        use rustpbx::proxy::proxy_call::sip_session::SipSession;

        let id = SessionId::from(session_id);
        let (handle, cmd_rx) = SipSession::with_handle(id);

        let entry = rustpbx::proxy::active_call_registry::ActiveProxyCallEntry {
            session_id: session_id.to_string(),
            caller: Some(caller.to_string()),
            callee: Some(callee.to_string()),
            direction: if matches!(direction, DialDirection::Inbound) {
                "inbound".to_string()
            } else {
                "outbound".to_string()
            },
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: rustpbx::proxy::active_call_registry::ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        (handle, cmd_rx)
    }

    #[tokio::test]
    async fn test_list_calls_empty() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert!(calls.is_empty());
        }
    }

    #[tokio::test]
    async fn test_answer_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_ring_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Ring {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_reject_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Reject {
                call_id: "nonexistent".into(),
                reason: None,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_attach_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::AttachCall {
                call_id: "nonexistent".into(),
                mode: rustpbx::rwi::session::OwnershipMode::Control,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::DetachCall {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_detach_call_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-to-detach",
            "caller1",
            "callee1",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());

        let result = processor
            .process_command(RwiCommandPayload::DetachCall {
                call_id: "call-to-detach".into(),
            })
            .await;

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), CommandResult::Success));

        assert!(registry.get_handle("call-to-detach").is_some());
    }

    #[tokio::test]
    async fn test_hangup_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Hangup {
                call_id: "nonexistent".into(),
                reason: Some("normal".into()),
                code: Some(16),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_transfer_call_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Transfer {
                call_id: "nonexistent".into(),
                target: "sip:target@local".into(),
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bridge_not_found_leg_a() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "missing-a".into(),
                leg_b: "missing-b".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_bridge_not_found_leg_b() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b-missing".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_bridge_both_legs_exist_sends_bridgeto() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let _ha = create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;

        match &result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_unbridge_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "nope".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_subscribe_success() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Subscribe {
                contexts: vec!["ctx1".into()],
                events: None,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_unsubscribe_success() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Unsubscribe {
                contexts: vec!["ctx1".into()],
            })
            .await;
        assert!(result.is_ok());
    }

    // ── call.set_var / call.get_var tests ─────────────────────────────────

    #[tokio::test]
    async fn test_set_var_returns_success() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SetVar {
                call_id: "call-1".into(),
                key: "greeting".into(),
                value: "hello".into(),
            })
            .await;
        assert!(matches!(result, Ok(CommandResult::Success)));
    }

    #[tokio::test]
    async fn test_get_var_returns_value_after_set() {
        let (processor, _cm) = create_test_processor();

        processor
            .process_command(RwiCommandPayload::SetVar {
                call_id: "call-1".into(),
                key: "lang".into(),
                value: "en".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::GetVar {
                call_id: "call-1".into(),
                key: "lang".into(),
            })
            .await
            .unwrap();

        assert!(
            matches!(&result, CommandResult::CallVar { key, value } if key == "lang" && value.as_deref() == Some("en")),
            "expected CallVar with value 'en', got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_get_var_returns_none_for_missing_key() {
        let (processor, _cm) = create_test_processor();

        let result = processor
            .process_command(RwiCommandPayload::GetVar {
                call_id: "call-1".into(),
                key: "nonexistent".into(),
            })
            .await
            .unwrap();

        assert!(
            matches!(&result, CommandResult::CallVar { key, value } if key == "nonexistent" && value.is_none()),
            "expected CallVar with None value, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_set_var_overwrites_existing() {
        let (processor, _cm) = create_test_processor();

        processor
            .process_command(RwiCommandPayload::SetVar {
                call_id: "call-1".into(),
                key: "x".into(),
                value: "first".into(),
            })
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::SetVar {
                call_id: "call-1".into(),
                key: "x".into(),
                value: "second".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::GetVar {
                call_id: "call-1".into(),
                key: "x".into(),
            })
            .await
            .unwrap();

        assert!(
            matches!(&result, CommandResult::CallVar { key, value } if key == "x" && value.as_deref() == Some("second")),
            "expected overwritten value 'second', got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_vars_are_isolated_per_call() {
        let (processor, _cm) = create_test_processor();

        processor
            .process_command(RwiCommandPayload::SetVar {
                call_id: "call-a".into(),
                key: "k".into(),
                value: "va".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::SetVar {
                call_id: "call-b".into(),
                key: "k".into(),
                value: "vb".into(),
            })
            .await
            .unwrap();

        let ra = processor
            .process_command(RwiCommandPayload::GetVar {
                call_id: "call-a".into(),
                key: "k".into(),
            })
            .await
            .unwrap();
        let rb = processor
            .process_command(RwiCommandPayload::GetVar {
                call_id: "call-b".into(),
                key: "k".into(),
            })
            .await
            .unwrap();

        assert!(
            matches!(&ra, CommandResult::CallVar { value, .. } if value.as_deref() == Some("va"))
        );
        assert!(
            matches!(&rb, CommandResult::CallVar { value, .. } if value.as_deref() == Some("vb"))
        );
    }

    #[tokio::test]
    async fn test_media_play_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaPlay(
                rustpbx::rwi::session::MediaPlayRequest {
                    call_id: "missing".into(),
                    source: rustpbx::rwi::session::MediaSource {
                        source_type: "file".into(),
                        uri: Some("welcome.wav".into()),
                        looped: None,
                    },
                    interrupt_on_dtmf: false,
                    leg_id: None,
                    loop_playback: false,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_originate_no_server_returns_error() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(
                rustpbx::rwi::session::OriginateRequest {
                    call_id: "new-call".into(),
                    destination: "sip:test@local".into(),
                    caller_id: None,
                    timeout_secs: Some(30),
                    extra_headers: std::collections::HashMap::new(),
                    trunk: None,
                    route_originated_calls: None,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_originate_invalid_destination_returns_error() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::Originate(
                rustpbx::rwi::session::OriginateRequest {
                    call_id: "new-call-2".into(),
                    destination: "not-a-sip-uri".into(),
                    caller_id: None,
                    timeout_secs: None,
                    extra_headers: std::collections::HashMap::new(),
                    trunk: None,
                    route_originated_calls: None,
                },
            ))
            .await;
        assert!(result.is_err());

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_originate_routes_when_enabled_and_reject_aborts() {
        use rustpbx::config::ProxyConfig;
        use rustpbx::proxy::routing::{
            MatchConditions, RejectConfig, RouteAction, RouteRule,
        };
        use crate::common::e2e_test_server::E2eTestServer;

        let mut config = ProxyConfig::default();
        config.route_originated_calls = false;
        config.routes = Some(vec![RouteRule {
            name: "reject-9".to_string(),
            priority: 100,
            match_conditions: MatchConditions {
                request_uri_user: Some("9.*".to_string()),
                ..Default::default()
            },
            action: RouteAction {
                reject: Some(RejectConfig {
                    code: 486,
                    reason: Some("blocked".to_string()),
                    headers: std::collections::HashMap::new(),
                }),
                ..Default::default()
            },
            ..Default::default()
        }]);

        let server = E2eTestServer::start_with_config(config).await.expect("start e2e server");
        let processor = RwiCommandProcessor::new(
            Arc::new(ActiveProxyCallRegistry::new()),
            Arc::new(RwLock::new(RwiGateway::new())),
            Arc::new(ConferenceManager::new()),
        )
        .with_sip_server(server.server_ref.clone());

        let result = processor
            .process_command(RwiCommandPayload::Originate(
                rustpbx::rwi::session::OriginateRequest {
                    call_id: "route-reject".into(),
                    destination: "sip:9001@rustpbx.com".into(),
                    caller_id: None,
                    timeout_secs: Some(5),
                    extra_headers: std::collections::HashMap::new(),
                    trunk: None,
                    route_originated_calls: Some(true),
                },
            ))
            .await;
        assert!(
            result.is_err(),
            "reject route must abort originate when routing is enabled"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("route aborted") && msg.contains("486"),
            "unexpected abort error: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_originate_explicit_trunk_skips_route() {
        use rustpbx::config::ProxyConfig;
        use rustpbx::proxy::routing::TrunkConfig;
        use rustpbx::proxy::routing::{
            MatchConditions, RejectConfig, RouteAction, RouteRule,
        };
        use crate::common::e2e_test_server::E2eTestServer;

        let mut config = ProxyConfig::default();
        config.route_originated_calls = true;
        config.routes = Some(vec![RouteRule {
            name: "reject-9".to_string(),
            priority: 100,
            match_conditions: MatchConditions {
                request_uri_user: Some("9.*".to_string()),
                ..Default::default()
            },
            action: RouteAction {
                reject: Some(RejectConfig {
                    code: 486,
                    reason: Some("blocked".to_string()),
                    headers: std::collections::HashMap::new(),
                }),
                ..Default::default()
            },
            ..Default::default()
        }]);
        let mut trunks = std::collections::HashMap::new();
        trunks.insert(
            "gw1".to_string(),
            TrunkConfig {
                dest: "sip:gateway.rustpbx.test:5060".to_string(),
                ..Default::default()
            },
        );
        config.trunks = trunks;

        let server = E2eTestServer::start_with_config(config).await.expect("start e2e server");
        let processor = RwiCommandProcessor::new(
            Arc::new(ActiveProxyCallRegistry::new()),
            Arc::new(RwLock::new(RwiGateway::new())),
            Arc::new(ConferenceManager::new()),
        )
        .with_sip_server(server.server_ref.clone());

        let result = processor
            .process_command(RwiCommandPayload::Originate(
                rustpbx::rwi::session::OriginateRequest {
                    call_id: "trunk-wins".into(),
                    destination: "sip:9001@rustpbx.com".into(),
                    caller_id: None,
                    timeout_secs: Some(2),
                    extra_headers: std::collections::HashMap::new(),
                    trunk: Some("gw1".to_string()),
                    route_originated_calls: Some(true),
                },
            ))
            .await;
        assert!(
            result.is_ok(),
            "explicit trunk must bypass the reject route, got: {:?}",
            result.as_ref().err()
        );
    }

    #[tokio::test]
    async fn test_answer_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(
            &registry,
            "call-001",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        assert!(registry.get_handle("call-001").is_some());

        let result = processor
            .process_command(RwiCommandPayload::Answer {
                call_id: "call-001".into(),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_hangup_existing_call() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let _handle = create_test_call(
            &registry,
            "call-001",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::Hangup {
                call_id: "call-001".into(),
                reason: Some("normal".into()),
                code: Some(16),
            })
            .await;
        match result {
            Ok(_) => {}
            Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_list_calls_with_multiple_calls() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());

        create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        create_test_call(&registry, "call-2", "1002", "2001", DialDirection::Outbound);
        create_test_call(&registry, "call-3", "1003", "2002", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        assert!(result.is_ok());
        if let Ok(CommandResult::ListCalls(calls)) = result {
            assert_eq!(calls.len(), 3);
            let ids: Vec<_> = calls.iter().map(|c| c.session_id.clone()).collect();
            assert!(ids.contains(&"call-1".to_string()));
            assert!(ids.contains(&"call-2".to_string()));
            assert!(ids.contains(&"call-3".to_string()));
        }
    }

    #[tokio::test]
    async fn test_call_direction_filtering() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());

        create_test_call(
            &registry,
            "inbound-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        create_test_call(
            &registry,
            "outbound-1",
            "2001",
            "1001",
            DialDirection::Outbound,
        );
        create_test_call(
            &registry,
            "inbound-2",
            "1002",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::ListCalls)
            .await;
        if let Ok(CommandResult::ListCalls(calls)) = result {
            let inbound: Vec<_> = calls.iter().filter(|c| c.direction == "inbound").collect();
            let outbound: Vec<_> = calls.iter().filter(|c| c.direction == "outbound").collect();
            assert_eq!(inbound.len(), 2);
            assert_eq!(outbound.len(), 1);
        }
    }

    #[tokio::test]
    async fn test_bridge_emits_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway.clone(),
            cm.clone(),
        ));

        let _ha = create_test_call(&registry, "leg-a", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b", "1001", "2002", DialDirection::Outbound);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut gw = gateway.write();
            let identity = rustpbx::rwi::auth::RwiIdentity {
                token: "t".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.claim_call_ownership(
                &sid,
                "leg-a".into(),
                rustpbx::rwi::session::OwnershipMode::Control,
            )
            .unwrap();
        }

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a".into(),
                leg_b: "leg-b".into(),
            })
            .await;

        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                match tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv()).await
                {
                    Ok(Some(ev)) => {
                        let s = serde_json::to_string(&ev).unwrap();
                        assert!(
                            s.contains("\"leg_a\"") && s.contains("\"leg_b\""),
                            "Expected call_bridged event, got: {}",
                            s
                        );
                    }
                    Ok(None) => panic!("Event channel closed unexpectedly"),
                    Err(_) => panic!(
                        "Timeout waiting for CallBridged event - event was not sent to gateway"
                    ),
                }
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_media_stop_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "ghost".into(),
                leg_id: None,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_media_stop_existing_call_sends_stop_playback() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-stop",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::MediaStop {
                call_id: "call-stop".into(),
                leg_id: None,
            })
            .await;

        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("StopPlayback should be queued");
        assert!(matches!(cmd, CallCommand::StopPlayback { .. }));
    }

    #[tokio::test]
    async fn test_unbridge_existing_call_sends_unbridge() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unb",
            "1001",
            "2000",
            DialDirection::Inbound,
        );

        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "call-unb".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx.try_recv().expect("Unbridge should be queued");
        assert!(matches!(cmd, CallCommand::Unbridge { .. }));
    }

    #[tokio::test]
    async fn test_bridge_sends_bridge_to_to_leg_a() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (processor, _cm) = create_test_processor_with_registry(registry.clone());
        let (_ha, mut rx_a) =
            create_test_call_with_rx(&registry, "leg-a2", "1001", "2001", DialDirection::Outbound);
        let _hb = create_test_call(&registry, "leg-b2", "1001", "2002", DialDirection::Outbound);

        let result = processor
            .process_command(RwiCommandPayload::Bridge {
                leg_a: "leg-a2".into(),
                leg_b: "leg-b2".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }

        let cmd = rx_a.try_recv().expect("Bridge should be queued on leg_a");
        assert!(
            matches!(cmd, CallCommand::Bridge { leg_a: _, ref leg_b, .. } if leg_b.as_str() == "leg-b2"),
            "expected Bridge(leg-b2), got {:?}",
            cmd
        );
    }

    #[tokio::test]
    async fn test_unbridge_emits_call_unbridged_event_to_gateway() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway.clone(),
            cm.clone(),
        ));

        let (_handle, _rx) =
            create_test_call_with_rx(&registry, "call-ev", "1001", "2000", DialDirection::Inbound);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let mut gw = gateway.write();
            let identity = rustpbx::rwi::auth::RwiIdentity {
                token: "t2".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.claim_call_ownership(
                &sid,
                "call-ev".into(),
                rustpbx::rwi::session::OwnershipMode::Control,
            )
            .unwrap();
        }

        let result = processor
            .process_command(RwiCommandPayload::Unbridge {
                call_id: "call-ev".into(),
            })
            .await;
        match result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {
                match tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv()).await
                {
                    Ok(Some(ev)) => {
                        let s = serde_json::to_string(&ev).unwrap();
                        assert!(s.contains("call-ev"), "Event should reference call-ev");
                    }
                    Ok(None) => panic!("Event channel closed unexpectedly"),
                    Err(_) => panic!(
                        "Timeout waiting for CallUnbridged event - event was not sent to gateway"
                    ),
                }
            }
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_set_ringback_source_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "call-target",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 = create_test_call(
            &registry,
            "call-source",
            "1002",
            "2001",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_ok(), "SetRingbackSource failed: {:?}", result);
    }

    #[tokio::test]
    async fn test_set_ringback_source_target_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-source",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "nonexistent".into(),
                source_call_id: "call-source".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_set_ringback_source_source_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-target",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SetRingbackSource {
                target_call_id: "call-target".into(),
                source_call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    /// `CommandDeduplicationCache::record` must opportunistically evict
    /// expired entries once the soft cap is exceeded, so dedup correctness is
    /// preserved without unbounded growth.
    #[tokio::test]
    async fn test_command_dedup_cache_evicts_expired_entries_above_soft_cap() {
        let cache = CommandDeduplicationCache::new(60);
        // Use a backdated `received_at` for inserted entries so they are
        // already expired when GC runs. We achieve this by inserting normally
        // then mutating `received_at` through the public API is not possible,
        // so instead we drive eviction through sheer count: cap is 256; we
        // insert 256 fresh entries (which are NOT yet expired) plus one more,
        // then verify the cache size stays bounded by the soft cap.
        for i in 0..COMMAND_DEDUP_SOFT_CAP {
            cache.record(format!("action-{i}"));
        }
        // None are expired yet, so the cache must hold all of them.
        assert_eq!(cache.len(), COMMAND_DEDUP_SOFT_CAP);
        // Recording one more triggers cleanup_expired; since nothing is
        // expired, size grows to SOFT_CAP + 1.
        cache.record("action-trigger".into());
        assert_eq!(cache.len(), COMMAND_DEDUP_SOFT_CAP + 1);

        // Now wait past TTL so subsequent records evict everything old.
        // Use a fresh short-TTL cache to keep the test fast and deterministic.
        let short = CommandDeduplicationCache::new(0);
        // TTL of 0 means any entry is immediately expired; record once to
        // populate, then push past the cap and confirm eviction occurs.
        for i in 0..(COMMAND_DEDUP_SOFT_CAP + 1) {
            short.record(format!("old-{i}"));
        }
        // With TTL=0 every entry is expired by the time we cross the cap,
        // so after the final record the cache must contain only that record.
        let len = short.len();
        assert!(len <= 1, "expected at most 1 entry after GC, got {}", len);
    }

    #[tokio::test]
    async fn test_record_start_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStart(
                rustpbx::rwi::session::RecordStartRequest {
                    call_id: "call-rec".into(),
                    mode: "local".into(),
                    beep: Some(true),
                    max_duration_secs: Some(3600),
                    storage: rustpbx::rwi::session::RecordStorage {
                        path: "/recordings/call-rec.wav".into(),
                    },
                },
            ))
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
        if let Ok(action) = cmd {
            assert!(matches!(action, CallCommand::StartRecording { .. }));
        }
    }

    #[tokio::test]
    async fn test_record_start_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::RecordStart(
                rustpbx::rwi::session::RecordStartRequest {
                    call_id: "nonexistent".into(),
                    mode: "local".into(),
                    beep: Some(true),
                    max_duration_secs: Some(3600),
                    storage: rustpbx::rwi::session::RecordStorage {
                        path: "/recordings/call.wav".into(),
                    },
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_record_pause_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-p",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                rustpbx::rwi::session::RecordStartRequest {
                    call_id: "call-rec-p".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: rustpbx::rwi::session::RecordStorage {
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-rec-p".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_record_pause_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-norec",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-norec".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recording"));
    }

    #[tokio::test]
    async fn test_record_resume_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-r",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                rustpbx::rwi::session::RecordStartRequest {
                    call_id: "call-rec-r".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: rustpbx::rwi::session::RecordStorage {
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::RecordPause {
                call_id: "call-rec-r".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::RecordResume {
                call_id: "call-rec-r".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_record_resume_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-norec2",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordResume {
                call_id: "call-norec2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No recording"));
    }

    #[tokio::test]
    async fn test_record_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-rec-s",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::RecordStart(
                rustpbx::rwi::session::RecordStartRequest {
                    call_id: "call-rec-s".into(),
                    mode: "local".into(),
                    beep: Some(false),
                    max_duration_secs: None,
                    storage: rustpbx::rwi::session::RecordStorage {
                        path: "/recordings/test.wav".into(),
                    },
                },
            ))
            .await
            .unwrap();

        let (stop_seen_tx, stop_seen_rx) = tokio::sync::oneshot::channel();
        rustpbx::utils::spawn(async move {
            while let Some(command) = rx.recv().await {
                if matches!(command, CallCommand::StopRecording) {
                    let _ = stop_seen_tx.send(());
                    break;
                }
            }
        });

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-rec-s".into(),
            })
            .await;
        assert!(result.is_ok());

        stop_seen_rx
            .await
            .expect("Expected StopRecording action to be sent");
    }

    #[tokio::test]
    async fn test_record_stop_no_recording() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-norec3",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::RecordStop {
                call_id: "call-norec3".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        if let Ok(action) = cmd {
            assert!(matches!(action, CallCommand::StopRecording));
        }
    }

    #[tokio::test]
    async fn test_queue_enqueue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, _rx) = create_test_call_with_rx(
            &registry,
            "call-q",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                rustpbx::rwi::session::QueueEnqueueRequest {
                    call_id: "call-q".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                },
            ))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_enqueue_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                rustpbx::rwi::session::QueueEnqueueRequest {
                    call_id: "nonexistent".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_queue_dequeue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, _rx) = create_test_call_with_rx(
            &registry,
            "call-dq",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                rustpbx::rwi::session::QueueEnqueueRequest {
                    call_id: "call-dq".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueDequeue {
                call_id: "call-dq".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_dequeue_not_found() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::QueueDequeue {
                call_id: "nonexistent".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_queue_hold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-hold",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                rustpbx::rwi::session::QueueEnqueueRequest {
                    call_id: "call-hold".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-hold".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_hold_not_in_queue() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-noq",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-noq".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Call not in queue")
        );
    }

    #[tokio::test]
    async fn test_call_hold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-hold-direct",
            "1001",
            "1002",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::CallHold {
                call_id: "call-hold-direct".into(),
                music: None,
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_call_unhold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unhold-direct",
            "1001",
            "1002",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::CallUnhold {
                call_id: "call-unhold-direct".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_unhold_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let (_handle, mut rx) = create_test_call_with_rx(
            &registry,
            "call-unhold",
            "1001",
            "support",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        processor
            .process_command(RwiCommandPayload::QueueEnqueue(
                rustpbx::rwi::session::QueueEnqueueRequest {
                    call_id: "call-unhold".into(),
                    queue_id: "support".into(),
                    priority: Some(5),
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::QueueHold {
                call_id: "call-unhold".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-unhold".into(),
            })
            .await;
        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));

        let cmd = rx.try_recv();
        assert!(cmd.is_ok());
    }

    #[tokio::test]
    async fn test_queue_unhold_not_in_queue() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(
            &registry,
            "call-noq2",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::QueueUnhold {
                call_id: "call-noq2".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Call not in queue")
        );
    }

    #[tokio::test]
    async fn test_supervisor_listen_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;

        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_listen_not_found() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorListen {
                supervisor_call_id: "nonexistent".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Call not found"));
    }

    #[tokio::test]
    async fn test_supervisor_whisper_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "call-1".into(),
            })
            .await;

        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_barge_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle1 = create_test_call(
            &registry,
            "supervisor-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        let _handle2 =
            create_test_call(&registry, "call-1", "1002", "2001", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorBarge {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
                agent_leg: "call-1".into(),
            })
            .await;

        match &result {
            Ok(_) | Err(CommandError::CommandFailed(_)) => {}
            Err(e) => panic!("Unexpected error: {}", e),
        }
    }

    #[tokio::test]
    async fn test_supervisor_stop_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SupervisorStop {
                supervisor_call_id: "supervisor-1".into(),
                target_call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sip_message_no_server() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipMessage {
                call_id: "call-1".into(),
                content_type: "text/plain".into(),
                body: "Hello".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_sip_notify_no_server() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipNotify {
                call_id: "call-1".into(),
                event: "check-sync".into(),
                content_type: "application/simple-message-summary".into(),
                body: "".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_sip_options_ping_no_server() {
        let (processor, _cm) = create_test_processor();
        let result = processor
            .process_command(RwiCommandPayload::SipOptionsPing {
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("SIP server not available")
        );
    }

    #[tokio::test]
    async fn test_conference_create_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    max_members: Some(10),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await;
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceCreated { conf_id }) => {
                assert_eq!(conf_id, "room-1");
            }
            _ => panic!("Expected ConferenceCreated result"),
        }
    }

    #[tokio::test]
    async fn test_conference_create_duplicate_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    max_members: None,
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    max_members: None,
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_conference_add_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"));
    }

    #[tokio::test]
    async fn test_conference_destroy_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    max_members: None,
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceDestroy {
                conf_id: "room-1".into(),
            })
            .await;
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceDestroyed { conf_id }) => {
                assert_eq!(conf_id, "room-1");
            }
            _ => panic!("Expected ConferenceDestroyed result"),
        }
    }

    #[tokio::test]
    async fn test_conference_destroy_not_found_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let processor = Arc::new(RwiCommandProcessor::new(
            registry,
            gateway,
            Arc::new(ConferenceManager::new()),
        ));

        let result = processor
            .process_command(RwiCommandPayload::ConferenceDestroy {
                conf_id: "nonexistent".into(),
            })
            .await;
        // destroy_conference is now idempotent: destroying a non-existent
        // conference succeeds (returns Ok + ConferenceDestroyed event).
        assert!(result.is_ok());
        match result {
            Ok(CommandResult::ConferenceDestroyed { conf_id }) => {
                assert_eq!(conf_id, "nonexistent");
            }
            _ => panic!("Expected ConferenceDestroyed result"),
        }
    }

    #[tokio::test]
    async fn test_conference_mute_not_in_conference_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    max_members: None,
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let _handle = create_test_call_with_conference_manager(
            &registry,
            "call-1",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        let result = processor
            .process_command(RwiCommandPayload::ConferenceMute {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("not found in conference") || error.contains("is not in conference")
        );
    }

    #[tokio::test]
    async fn test_conference_add_with_max_members() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-1".into(),
                    max_members: Some(2),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let _handle1 = create_test_call_with_conference_manager(
            &registry,
            "call-1",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-1".into(),
            })
            .await;
        assert!(result.is_ok());

        let _handle2 = create_test_call_with_conference_manager(
            &registry,
            "call-2",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-2".into(),
            })
            .await;
        assert!(result.is_ok());

        let _handle3 = create_test_call_with_conference_manager(
            &registry,
            "call-3",
            "1003",
            "2002",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let result = processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-1".into(),
                call_id: "call-3".into(),
            })
            .await;
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("maximum capacity") || error.contains("is full"));
    }

    #[tokio::test]
    async fn test_transfer_attended_returns_consultation_call_id() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(
            &registry,
            "call-attended-1",
            "1001",
            "2000",
            DialDirection::Inbound,
        );
        registry.update("call-attended-1", |entry| {
            entry.answered_at = Some(chrono::Utc::now());
            entry.status = rustpbx::proxy::active_call_registry::ActiveProxyCallStatus::Talking;
        });

        let result = processor
            .process_command(RwiCommandPayload::TransferAttended {
                call_id: "call-attended-1".into(),
                target: "sip:consult@local".into(),
                timeout_secs: Some(30),
            })
            .await;

        assert!(result.is_ok());
        match result.unwrap() {
            CommandResult::TransferAttended {
                original_call_id,
                consultation_call_id,
            } => {
                assert_eq!(original_call_id, "call-attended-1");
                assert!(!consultation_call_id.is_empty());
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_conference_seat_replace_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-1".into(),
                    max_members: Some(2),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let _handle_a = create_test_call_with_conference_manager(
            &registry,
            "call-a",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_a1 = create_test_call_with_conference_manager(
            &registry,
            "call-a1",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-1".into(),
                call_id: "call-a".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceSeatReplace {
                conf_id: "room-seat-1".into(),
                old_call_id: "call-a".into(),
                new_call_id: "call-a1".into(),
            })
            .await;
        assert!(result.is_ok());

        let manager = processor.conference_manager();
        let conf = manager
            .get_conference(&"room-seat-1".into())
            .await
            .expect("conference should exist");
        assert!(!conf.participants.contains_key(&LegId::new("call-a")));
        assert!(conf.participants.contains_key(&LegId::new("call-a1")));
    }

    #[tokio::test]
    async fn test_conference_seat_replace_failure_rolls_back_old_member() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-2".into(),
                    max_members: Some(3),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-3".into(),
                    max_members: Some(2),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let _handle_a = create_test_call_with_conference_manager(
            &registry,
            "call-a",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_b = create_test_call_with_conference_manager(
            &registry,
            "call-b",
            "1003",
            "2002",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_a1 = create_test_call_with_conference_manager(
            &registry,
            "call-a1",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-2".into(),
                call_id: "call-a".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-2".into(),
                call_id: "call-b".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-3".into(),
                call_id: "call-a1".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceSeatReplace {
                conf_id: "room-seat-2".into(),
                old_call_id: "call-a".into(),
                new_call_id: "call-a1".into(),
            })
            .await;
        assert!(result.is_err());

        let manager = processor.conference_manager();
        let conf = manager
            .get_conference(&"room-seat-2".into())
            .await
            .expect("conference should exist");
        assert!(conf.participants.contains_key(&LegId::new("call-a")));
        assert!(conf.participants.contains_key(&LegId::new("call-b")));
        assert!(!conf.participants.contains_key(&LegId::new("call-a1")));
    }

    #[tokio::test]
    async fn test_conference_seat_replace_failure_emits_failed_event() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());

        let mut gateway_impl = RwiGateway::new();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        gateway_impl.set_session_event_sender(&"test-session".to_string(), event_tx);
        let gateway = Arc::new(RwLock::new(gateway_impl));

        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-4".into(),
                    max_members: Some(3),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        processor
            .process_command(RwiCommandPayload::ConferenceCreate(
                ConferenceCreateRequest {
                    conf_id: "room-seat-5".into(),
                    max_members: Some(2),
                    host_call_id: None,
                    max_duration_secs: None,
                },
            ))
            .await
            .unwrap();

        let _handle_a = create_test_call_with_conference_manager(
            &registry,
            "call-a",
            "1001",
            "2000",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_b = create_test_call_with_conference_manager(
            &registry,
            "call-b",
            "1003",
            "2002",
            DialDirection::Inbound,
            Some(cm.clone()),
        );
        let _handle_a1 = create_test_call_with_conference_manager(
            &registry,
            "call-a1",
            "1002",
            "2001",
            DialDirection::Inbound,
            Some(cm.clone()),
        );

        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-4".into(),
                call_id: "call-a".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-4".into(),
                call_id: "call-b".into(),
            })
            .await
            .unwrap();
        processor
            .process_command(RwiCommandPayload::ConferenceAdd {
                conf_id: "room-seat-5".into(),
                call_id: "call-a1".into(),
            })
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::ConferenceSeatReplace {
                conf_id: "room-seat-4".into(),
                old_call_id: "call-a".into(),
                new_call_id: "call-a1".into(),
            })
            .await;
        assert!(result.is_err());

        let mut found = false;
        while let Ok(event) = event_rx.try_recv() {
            if event.get("reason").is_some() {
                found = true;
                break;
            }
        }
        assert!(found, "Expected conference_seat_replace_failed event");
    }

    #[tokio::test]
    async fn test_queue_set_priority_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
            }))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueSetPriority {
                call_id: "call-1".into(),
                priority: 10,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_set_priority_not_in_queue_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);

        let result = processor
            .process_command(RwiCommandPayload::QueueSetPriority {
                call_id: "call-1".into(),
                priority: 10,
            })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not in queue"));
    }

    #[tokio::test]
    async fn test_queue_assign_agent_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
            }))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueAssignAgent {
                call_id: "call-1".into(),
                agent_id: "agent-42".into(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_queue_requeue_success() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(RwiGateway::new()));
        let cm = Arc::new(ConferenceManager::new());
        let processor = Arc::new(RwiCommandProcessor::new(
            registry.clone(),
            gateway,
            cm.clone(),
        ));

        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        processor
            .process_command(RwiCommandPayload::QueueEnqueue(QueueEnqueueRequest {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                priority: None,
            }))
            .await
            .unwrap();

        let result = processor
            .process_command(RwiCommandPayload::QueueRequeue {
                call_id: "call-1".into(),
                queue_id: "sales".into(),
                priority: Some(5),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sip_message_send() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SipMessage {
                call_id: "call-1".into(),
                content_type: "text/plain".into(),
                body: "Hello".into(),
            })
            .await;

        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));
    }

    #[tokio::test]
    async fn test_sip_notify_send() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SipNotify {
                call_id: "call-1".into(),
                event: "refer".into(),
                content_type: "message/sipfrag".into(),
                body: "SIP/2.0 200 OK".into(),
            })
            .await;

        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));
    }

    #[tokio::test]
    async fn test_sip_options_ping() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let _handle = create_test_call(&registry, "call-1", "1001", "2000", DialDirection::Inbound);
        let (processor, _cm) = create_test_processor_with_registry(registry);

        let result = processor
            .process_command(RwiCommandPayload::SipOptionsPing {
                call_id: "call-1".into(),
            })
            .await;

        assert!(result.is_ok() || matches!(result, Err(CommandError::CommandFailed(_))));
    }
