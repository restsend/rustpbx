#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;
    use axum::{Json, Router, routing::post};
    use rustpbx::call::{DialStrategy, DialplanFlow, SipUser, TransactionCookie};
    use rustpbx::config::{HttpRouterConfig, MediaProxyMode, RtpConfig};
    use rustpbx::proxy::call::CallRouter;
    use rustpbx::proxy::routing::http::HttpCallRouter;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    struct StubRouteInvite;
    #[async_trait::async_trait]
    impl rustpbx::call::RouteInvite for StubRouteInvite {
        async fn route_invite(
            &self,
            _: rsipstack::dialog::invitation::InviteOption,
            _: &rsipstack::sip::Request,
            _: &rustpbx::call::DialDirection,
            _: &TransactionCookie,
        ) -> anyhow::Result<rustpbx::config::RouteResult> {
            Ok(rustpbx::config::RouteResult::NotHandled(
                rsipstack::dialog::invitation::InviteOption::default(),
                None,
            ))
        }
    }

    /// Trunk registry holding `trunks`.
    async fn data_context(
        trunks: std::collections::HashMap<String, rustpbx::proxy::routing::TrunkConfig>,
    ) -> Arc<rustpbx::proxy::data::ProxyDataContext> {
        let config = rustpbx::config::ProxyConfig { trunks, ..Default::default() };
        Arc::new(
            rustpbx::proxy::data::ProxyDataContext::new(Arc::new(config), None)
                .await
                .unwrap(),
        )
    }

    /// Resolve a routing decision for a synthetic INVITE against a stub
    /// route server that always replies with `response`.
    async fn resolve_with_response(
        response: serde_json::Value,
    ) -> Result<rustpbx::call::Dialplan, rustpbx::proxy::call::RouteError> {
        resolve_with_trunks(response, Default::default()).await
    }

    /// As [`resolve_with_response`], with `trunks` configured.
    async fn resolve_with_trunks(
        response: serde_json::Value,
        trunks: std::collections::HashMap<String, rustpbx::proxy::routing::TrunkConfig>,
    ) -> Result<rustpbx::call::Dialplan, rustpbx::proxy::call::RouteError> {
        resolve_inbound(response, trunks, None).await
    }

    /// As [`resolve_with_trunks`], for a call that arrived on `source_trunk`.
    async fn resolve_inbound(
        response: serde_json::Value,
        trunks: std::collections::HashMap<String, rustpbx::proxy::routing::TrunkConfig>,
        source_trunk: Option<&str>,
    ) -> Result<rustpbx::call::Dialplan, rustpbx::proxy::call::RouteError> {
        let cookie = TransactionCookie::default();
        if let Some(name) = source_trunk {
            cookie.insert_extension(rustpbx::call::TrunkContext {
                id: None,
                name: name.to_string(),
                did_numbers: Vec::new(),
            });
        }
        let app = Router::new().route(
            "/route",
            post(move || async move { Json(response.clone()) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };
        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(trunks).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("record-start-at-test".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };
        let caller = SipUser {
            username: "caller".to_string(),
            realm: Some("example.com".to_string()),
            from: Some("sip:caller@example.com".try_into().unwrap()),
            ..Default::default()
        };
        router
            .resolve(
                &request,
                Box::new(StubRouteInvite),
                &caller,
                &cookie,
            )
            .await
    }

    #[tokio::test]
    async fn test_http_router_per_target_retry_policy_and_legacy_compatibility() {
        let plan = resolve_with_response(json!({
            "action": "forward",
            "targets": [
                "sip:first@example.com",
                {"uri": "sip:second@example.com", "retry_policy": {"status_codes": [408, 503]}},
                {"uri": "sip:last@example.com", "retry_policy": {"status_codes": []}}
            ]
        })).await.unwrap();
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = plan.flow else {
            panic!("expected sequential targets");
        };
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[0].location.aor.to_string(), "sip:first@example.com");
        assert_eq!(attempts[1].location.aor.to_string(), "sip:second@example.com");
        assert!(attempts[0].retry_policy.is_none());
        assert_eq!(attempts[1].retry_policy.as_ref().unwrap().status_codes, vec![408, 503]);
        assert!(attempts[2].retry_policy.as_ref().unwrap().status_codes.is_empty());

        let legacy = resolve_with_response(json!({
            "action": "forward", "targets": ["sip:first@example.com", {"uri": "sip:last@example.com"}]
        })).await.unwrap();
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = legacy.flow else {
            panic!("expected sequential targets");
        };
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().all(|attempt| attempt.retry_policy.is_none()));
    }

    #[tokio::test]
    async fn test_http_router_legacy_targets_skip_invalid_uri() {
        let plan = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:first@example.com", "", "sip:last@example.com"]
        })).await.unwrap();
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = plan.flow else {
            panic!("expected sequential targets");
        };
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[1].location.aor.to_string(), "sip:last@example.com");
        assert!(attempts.iter().all(|attempt| attempt.retry_policy.is_none()));
    }

    #[tokio::test]
    async fn test_http_router_rtp_override_and_parallel_targets() {
        let plan = resolve_with_response(json!({
            "action": "forward", "strategy": "parallel",
            "targets": ["sip:first@example.com", {"uri": "sip:last@example.com"}],
            "media_proxy": "all",
            "rtp_config": {"bind_ip": "127.0.0.2", "external_ip": "2001:db8::1"}
        })).await.unwrap();
        assert_eq!(plan.media.bind_ip.as_deref(), Some("127.0.0.2"));
        assert_eq!(plan.media.external_ip.as_deref(), Some("2001:db8::1"));
        assert!(plan.media.enable_latching);
        assert_eq!(plan.media.proxy_mode, MediaProxyMode::All);
        let DialplanFlow::Targets(DialStrategy::Parallel(targets)) = plan.flow else {
            panic!("expected parallel targets");
        };
        assert_eq!(targets.len(), 2);
    }

    #[tokio::test]
    async fn test_http_router_rejects_invalid_retry_and_rtp_config() {
        for fields in [
            json!({"targets": [{"uri": "sip:test@example.com", "retry_policy": {"status_codes": [200]}}]}),
            json!({"targets": [{"uri": "sip:test@example.com", "retry_policy": {"status_codes": [700]}}]}),
            json!({"rtp_config": {"bind_ip": "invalid-ip"}}),
            json!({"rtp_config": {"external_ip": "invalid-ip"}}),
            json!({"rtp_config": {"bind_ip_typo": "127.0.0.2"}}),
        ] {
            let mut response = json!({"action": "forward", "targets": ["sip:test@example.com"]});
            response.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
            let error = resolve_with_response(response).await.err().expect("invalid config must fail");
            assert_eq!(error.status, Some(rsipstack::sip::StatusCode::ServerInternalError));
        }
    }

    #[tokio::test]
    async fn test_http_router_forward() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        let app = Router::new().route(
            "/route",
            post(move |Json(payload): Json<serde_json::Value>| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(payload).await;
                    Json(json!({
                        "action": "forward",
                        "targets": ["sip:1001@127.0.0.1"],
                        "strategy": "sequential",
                        "record": true,
                        "timeout": 30
                    }))
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-call-id".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: b"v=0\r\nc=IN IP4 127.0.0.1\r\nm=audio 4000 RTP/AVP 0".to_vec(),
        };

        let caller = SipUser {
            username: "caller".to_string(),
            realm: Some("example.com".to_string()),
            from: Some("sip:caller@example.com".try_into().unwrap()),
            ..Default::default()
        };

        let cookie = TransactionCookie::default();

        // Use a dummy trait object for RouteInvite if resolve doesn't use it
        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let route_invite = Box::new(DummyRouteInvite);

        let dialplan = router
            .resolve(&request, route_invite, &caller, &cookie)
            .await
            .unwrap();

        assert!(dialplan.recording.enabled);
        assert_eq!(
            dialplan.recording_policy.as_ref().and_then(|p| p.enabled),
            Some(true)
        );
        assert_eq!(dialplan.max_call_duration.unwrap().as_secs(), 30);

        let payload = rx.recv().await.unwrap();
        assert_eq!(payload["call_id"], "test-call-id");
        assert_eq!(payload["from"], "sip:caller@example.com");
        assert_eq!(payload["to"], "sip:target@example.com");
    }

    #[tokio::test]
    async fn test_http_router_record_false_sets_policy_disable() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "forward",
                    "targets": ["sip:1001@127.0.0.1"],
                    "record": false
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-record-false".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser {
            username: "caller".to_string(),
            realm: Some("example.com".to_string()),
            from: Some("sip:caller@example.com".try_into().unwrap()),
            ..Default::default()
        };

        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let dialplan = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await
            .unwrap();

        assert!(!dialplan.recording.enabled);
        assert_eq!(
            dialplan.recording_policy.as_ref().and_then(|p| p.enabled),
            Some(false)
        );
    }

    /// `record_start_at: "answer"` must flow into the per-call recording
    /// policy as `RecordingAutoStartAt::Answer` — recording then waits for
    /// the final 200 OK instead of starting on early media.
    #[tokio::test]
    async fn test_http_router_record_start_at_answer_merges_into_policy() {
        let dialplan = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "record": true,
            "record_start_at": "answer"
        }))
        .await
        .unwrap();

        let policy = dialplan
            .recording_policy
            .expect("record=true must set a policy");
        assert_eq!(policy.enabled, Some(true));
        assert_eq!(
            policy.auto_start_at,
            Some(rustpbx::config::RecordingAutoStartAt::Answer)
        );
    }

    /// `record_start_at: "media"` must parse to the early-media start point.
    #[tokio::test]
    async fn test_http_router_record_start_at_media_merges_into_policy() {
        let dialplan = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "record": true,
            "record_start_at": "media"
        }))
        .await
        .unwrap();

        let policy = dialplan
            .recording_policy
            .expect("record=true must set a policy");
        assert_eq!(
            policy.auto_start_at,
            Some(rustpbx::config::RecordingAutoStartAt::Media)
        );
    }

    /// Omitting `record_start_at` leaves `auto_start_at` unset so the call
    /// inherits the global recording policy's start point.
    #[tokio::test]
    async fn test_http_router_record_without_start_at_inherits_global() {
        let dialplan = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "record": true
        }))
        .await
        .unwrap();

        let policy = dialplan
            .recording_policy
            .expect("record=true must set a policy");
        assert_eq!(policy.enabled, Some(true));
        assert_eq!(policy.auto_start_at, None);
    }

    /// An unknown `record_start_at` value must fail deserialization — the
    /// route cannot silently fall back to a different recording point.
    #[tokio::test]
    async fn test_http_router_record_start_at_invalid_value_rejected() {
        let result = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "record": true,
            "record_start_at": "whenever"
        }))
        .await;

        assert!(result.is_err(), "invalid record_start_at must not resolve");
    }

    #[tokio::test]
    async fn test_http_router_reject() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "reject",
                    "status": 403,
                    "reason": "Forbidden by test"
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-call-id".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser {
            username: "caller".to_string(),
            realm: Some("example.com".to_string()),
            from: Some("sip:caller@example.com".try_into().unwrap()),
            ..Default::default()
        };

        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let route_invite = Box::new(DummyRouteInvite);

        let result = router
            .resolve(&request, route_invite, &caller, &cookie)
            .await;

        match result {
            Err(err) => {
                assert_eq!(err.status, Some(rsipstack::sip::StatusCode::Forbidden));
                assert!(err.error.to_string().contains("Forbidden by test"));
                // The unified call-error catalog stamps `error_code` onto the
                // extensions even when the HTTP payload provided none.
                let exts = err.extensions.expect(
                    "extensions should carry the catalog error_code when not provided in reject",
                );
                assert_eq!(
                    exts.get("error_code").map(String::as_str),
                    Some("http_router.rejected")
                );
                assert_eq!(exts.len(), 1, "no other extensions invented in reject");
            }
            _ => panic!("Expected rejection"),
        }
    }

    #[tokio::test]
    async fn test_http_router_abort_preserves_extensions() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "abort",
                    "status": 503,
                    "reason": "Service unavailable",
                    "extensions": {
                        "reason_code": "maintenance",
                        "retry_after": "3600"
                    }
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-abort-ext".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser {
            username: "caller".to_string(),
            realm: Some("example.com".to_string()),
            from: Some("sip:caller@example.com".try_into().unwrap()),
            ..Default::default()
        };

        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let result = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await;

        let err = result.expect_err("abort should return error");
        assert_eq!(
            err.status,
            Some(rsipstack::sip::StatusCode::ServiceUnavailable)
        );
        assert!(err.error.to_string().contains("Service unavailable"));
        let exts = err
            .extensions
            .expect("extensions should be preserved on abort");
        assert_eq!(exts.get("reason_code").unwrap(), "maintenance");
        assert_eq!(exts.get("retry_after").unwrap(), "3600");
    }

    #[tokio::test]
    async fn test_http_router_reject_preserves_extensions() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "reject",
                    "status": 403,
                    "reason": "Blocked",
                    "extensions": {
                        "block_reason": "blacklist",
                        "source_ip": "10.0.0.1"
                    }
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-reject-ext".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser::default();
        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let result = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await;

        let err = result.expect_err("reject should return error");
        assert_eq!(err.status, Some(rsipstack::sip::StatusCode::Forbidden));
        assert!(err.error.to_string().contains("Blocked"));
        let exts = err
            .extensions
            .expect("extensions should be preserved on reject");
        assert_eq!(exts.get("block_reason").unwrap(), "blacklist");
        assert_eq!(exts.get("source_ip").unwrap(), "10.0.0.1");
    }

    #[tokio::test]
    async fn test_http_router_abort_without_extensions() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "abort",
                    "status": 486,
                    "reason": "Busy"
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-abort-noext".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser::default();
        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let result = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await;

        let err = result.expect_err("abort should return error");
        assert_eq!(err.status, Some(rsipstack::sip::StatusCode::BusyHere));
        assert!(err.error.to_string().contains("Busy"));
        // The unified call-error catalog stamps `error_code` onto the
        // extensions even when the HTTP payload provided none.
        let exts = err
            .extensions
            .expect("extensions should carry the catalog error_code when not provided");
        assert_eq!(
            exts.get("error_code").map(String::as_str),
            Some("http_router.rejected")
        );
        assert_eq!(exts.len(), 1, "no other extensions invented");
    }

    #[tokio::test]
    async fn test_http_router_enhanced() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "forward",
                    "targets": ["sip:1001@127.0.0.1"],
                    "media_proxy": "none",
                    "headers": {
                        "X-Custom-Header": "test-value"
                    },
                    "extensions": {
                        "custom_id": "123456"
                    },
                    "with_original_headers": true
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-id".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser::default();
        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let dialplan = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await
            .unwrap();

        assert_eq!(
            dialplan.media.proxy_mode,
            rustpbx::config::MediaProxyMode::None
        );
        assert!(
            dialplan.header_passthrough.is_some(),
            "with_original_headers=true must map to a passthrough rule"
        );
        assert!(
            dialplan
                .header_passthrough
                .as_ref()
                .unwrap()
                .allows("X-Smart2Agent"),
            "with_original_headers=true must be an all-forwarding rule"
        );

        let target = dialplan.first_target().unwrap();
        let header = target.headers.as_ref().unwrap().first().unwrap();
        assert_eq!(header.to_string(), "X-Custom-Header: test-value");

        let exts = dialplan
            .extensions
            .get::<std::collections::HashMap<String, String>>()
            .unwrap();
        assert_eq!(exts.get("custom_id").unwrap(), "123456");
    }

    /// Verify that the HTTP router applies the server's RTP config (external_ip, port range)
    /// to the dialplan so that SDP generation uses the correct public IP.
    #[tokio::test]
    async fn test_http_router_applies_rtp_config_to_dialplan() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "forward",
                    "targets": ["sip:1001@127.0.0.1"],
                    "media_proxy": "all",
                    "rtp_config": {"bind_ip": "127.0.0.2"}
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let rtp_config = RtpConfig {
            external_ip: Some("203.0.113.1".to_string()),
            bind_ip: Some("127.0.0.1".to_string()),
            start_port: Some(10000),
            end_port: Some(20000),
            ..Default::default()
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(rtp_config)),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-rtp-config".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser::default();
        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let dialplan = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await
            .unwrap();

        // Verify media_proxy override from HTTP router response
        assert_eq!(
            dialplan.media.proxy_mode,
            rustpbx::config::MediaProxyMode::All
        );
        assert_eq!(dialplan.media.bind_ip.as_deref(), Some("127.0.0.2"));
        // Omitted RTP fields still inherit server settings
        assert_eq!(dialplan.media.external_ip.as_deref(), Some("203.0.113.1"));
        assert_eq!(dialplan.media.rtp_start_port, Some(10000));
        assert_eq!(dialplan.media.rtp_end_port, Some(20000));
    }

    #[tokio::test]
    async fn test_http_router_forward_parallel_multi_target() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "forward",
                    "targets": [
                        "sip:1001@example.com",
                        "sip:1002@example.com",
                        "sip:1003@example.com"
                    ],
                    "strategy": "parallel"
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".into()),
                rsipstack::sip::Header::To("sip:target@example.com".into()),
                rsipstack::sip::Header::CallId("test-parallel-call-id".into()),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };

        let caller = SipUser {
            username: "caller".to_string(),
            realm: Some("example.com".to_string()),
            from: Some("sip:caller@example.com".try_into().unwrap()),
            ..Default::default()
        };

        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let route_invite = Box::new(DummyRouteInvite);

        let dialplan = router
            .resolve(&request, route_invite, &caller, &cookie)
            .await
            .unwrap();

        // Verify Parallel strategy with 3 targets
        match dialplan.flow {
            DialplanFlow::Targets(strategy) => match strategy {
                DialStrategy::Parallel(targets) => {
                    assert_eq!(targets.len(), 3);
                    assert_eq!(targets[0].aor.to_string(), "sip:1001@example.com");
                    assert_eq!(targets[1].aor.to_string(), "sip:1002@example.com");
                    assert_eq!(targets[2].aor.to_string(), "sip:1003@example.com");
                    // Verify all targets have no destination (will be resolved later)
                    for target in &targets {
                        assert!(
                            target.destination.is_none(),
                            "HTTP router targets should have no destination set"
                        );
                    }
                }
                _ => panic!("Expected DialStrategy::Parallel"),
            },
            _ => panic!("Expected DialplanFlow::Targets"),
        }
    }

    #[tokio::test]
    async fn test_http_router_with_original_headers_false_disables_passthrough() {
        let app = Router::new().route(
            "/route",
            post(|| async {
                Json(json!({
                    "action": "forward",
                    "targets": ["sip:1001@127.0.0.1"],
                    "with_original_headers": false
                }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        rustpbx::utils::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };
        let router = HttpCallRouter::new(
            config,
            ArcSwap::new(Arc::new(RtpConfig::default())),
            ArcSwap::new(Arc::new(MediaProxyMode::None)),
            true,
            None,
            data_context(Default::default()).await,
        );

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: Default::default(),
            version: rsipstack::sip::Version::V2,
            body: vec![],
        };
        let caller = SipUser::default();
        let cookie = TransactionCookie::default();

        struct DummyRouteInvite;
        #[async_trait::async_trait]
        impl rustpbx::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &rustpbx::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<rustpbx::config::RouteResult> {
                Ok(rustpbx::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
        }

        let dialplan = router
            .resolve(&request, Box::new(DummyRouteInvite), &caller, &cookie)
            .await
            .unwrap();

        let rule = dialplan
            .header_passthrough
            .as_ref()
            .expect("with_original_headers=false must be an explicit rule");
        assert!(
            !rule.allows("X-Smart2Agent"),
            "with_original_headers=false must forward no custom headers"
        );
    }

    #[tokio::test]
    async fn test_http_router_forward_caller_rewrite_full_uri() {
        let dialplan = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "caller": "sip:88888888@pbx.example.com"
        }))
        .await
        .unwrap();

        let caller = dialplan.caller.expect("dialplan caller must be set");
        assert_eq!(caller.to_string(), "sip:88888888@pbx.example.com");
    }

    #[tokio::test]
    async fn test_http_router_forward_caller_rewrite_bare_user() {
        let dialplan = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "caller": "88888888"
        }))
        .await
        .unwrap();

        let caller = dialplan.caller.expect("dialplan caller must be set");
        assert_eq!(
            caller.to_string(),
            "sip:88888888@example.com",
            "bare user must be qualified with the caller's realm"
        );
    }

    #[tokio::test]
    async fn test_http_router_forward_caller_rewrite_invalid_fails_call() {
        let result = resolve_with_response(json!({
            "action": "forward",
            "targets": ["sip:1001@127.0.0.1"],
            "caller": "sip:88888888@pbx.example.com:99999"
        }))
        .await;

        let err = result.expect_err("an invalid caller override must fail the call");
        assert!(
            err.error.to_string().contains("88888888"),
            "error should mention the invalid caller override: {}",
            err.error
        );
        assert!(
            err.status.is_some(),
            "an invalid caller override must carry a SIP error status"
        );
    }

    fn trunk(dest: &str, retry_codes: Option<Vec<u16>>) -> rustpbx::proxy::routing::TrunkConfig {
        rustpbx::proxy::routing::TrunkConfig {
            dest: dest.to_string(),
            username: Some("acct".to_string()),
            password: Some("secret".to_string()),
            max_ring_time: Some(20),
            retry_policy: retry_codes
                .map(|status_codes| rustpbx::proxy::routing::RetryPolicy { status_codes }),
            ..Default::default()
        }
    }

    fn outbound_trunk(attempt: &rustpbx::call::DialAttempt) -> String {
        attempt
            .hints
            .as_ref()
            .and_then(|hints| hints.extensions.get::<rustpbx::call::OutboundTrunkContext>())
            .map(|ctx| ctx.name.clone())
            .expect("trunk attempt must carry its outbound trunk")
    }

    #[tokio::test]
    async fn test_http_router_trunk_targets_inherit_trunk_config_and_retry() {
        let trunks = [
            ("carrier-a".to_string(), trunk("sip:10.0.0.1:5060", Some(vec![408, 503]))),
            ("carrier-b".to_string(), trunk("sip:10.0.0.2:5080", None)),
        ]
        .into();
        let plan = resolve_with_trunks(json!({
            "action": "forward",
            "caller": "sip:cli@example.com",
            "targets": [
                {"trunk": "carrier-a"},
                {"trunk": "carrier-b", "callee": "8613800000000"},
                {"trunk": "carrier-b", "retry_policy": {"status_codes": [486]}},
                "sip:legacy@example.com"
            ]
        }), trunks).await.unwrap();
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = plan.flow else {
            panic!("expected sequential targets");
        };
        assert_eq!(attempts.len(), 4);

        // Callee defaults to the dialed user, qualified with the trunk's host;
        // destination and credentials come from the trunk.
        assert_eq!(attempts[0].location.aor.to_string(), "sip:target@10.0.0.1:5060");
        assert_eq!(attempts[0].location.destination.as_ref().unwrap().addr.to_string(), "10.0.0.1:5060");
        assert_eq!(attempts[0].location.credential.as_ref().unwrap().username, "acct");
        assert_eq!(attempts[1].location.aor.to_string(), "sip:8613800000000@10.0.0.2:5080");
        assert_eq!(outbound_trunk(&attempts[0]), "carrier-a");
        assert_eq!(outbound_trunk(&attempts[1]), "carrier-b");
        assert_eq!(
            attempts[0].hints.as_ref().unwrap().max_ring_time,
            Some(std::time::Duration::from_secs(20))
        );
        // The response's caller override is the trunk attempt's caller, with
        // the trunk's host rewrite applied.
        assert_eq!(attempts[0].caller.as_ref().unwrap().to_string(), "sip:cli@10.0.0.1:5060");

        // Retry: the trunk's policy, an empty one when it has none, or the
        // target's own override; plain URIs keep legacy advance-on-failure.
        assert_eq!(attempts[0].retry_policy.as_ref().unwrap().status_codes, vec![408, 503]);
        assert!(attempts[1].retry_policy.as_ref().unwrap().status_codes.is_empty());
        assert_eq!(attempts[2].retry_policy.as_ref().unwrap().status_codes, vec![486]);
        assert!(attempts[3].retry_policy.is_none());
        // Ring time limits each attempt in a sequence, not the call.
        assert_eq!(plan.max_ring_time, None);
    }

    #[tokio::test]
    async fn test_http_router_single_trunk_target_ring_time_limits_call() {
        let trunks = [("carrier-a".to_string(), trunk("sip:10.0.0.1:5060", None))].into();
        let plan = resolve_with_trunks(
            json!({"action": "forward", "targets": [{"trunk": "carrier-a"}]}),
            trunks,
        )
        .await
        .unwrap();
        assert_eq!(plan.max_ring_time, Some(std::time::Duration::from_secs(20)));
    }

    #[tokio::test]
    async fn test_http_router_rejects_bad_trunk_targets() {
        let mut disabled = trunk("sip:10.0.0.3", None);
        disabled.disabled = Some(true);
        let trunks: std::collections::HashMap<_, _> = [
            ("carrier-a".to_string(), trunk("sip:10.0.0.1", None)),
            ("off".to_string(), disabled),
        ]
        .into();
        for target in [
            json!({"trunk": "missing"}),
            json!({"trunk": "off"}),
            json!({"trunk": "carrier-a", "uri": "sip:x@example.com"}),
            json!({"trunk": "carrier-a", "retry_policy": {"status_codes": [200]}}),
        ] {
            let result = resolve_with_trunks(
                json!({"action": "forward", "targets": [target.clone()]}),
                trunks.clone(),
            )
            .await;
            assert!(result.is_err(), "{target} must fail the call");
        }
    }

    #[tokio::test]
    async fn test_http_router_arrival_trunk_network_path_under_target_trunk() {
        let mut arrival = trunk("sip:192.0.2.9", None);
        arrival.profile = Some("arrival".to_string());
        arrival.external_ip = Some("198.51.100.1".to_string());
        arrival.codec = vec!["pcma".to_string()];
        let mut own = trunk("sip:10.0.0.2", None);
        own.profile = Some("dest".to_string());
        let trunks = [
            ("carrier-in".to_string(), arrival),
            ("own-profile".to_string(), own),
            ("no-profile".to_string(), trunk("sip:10.0.0.3", None)),
        ]
        .into();
        let plan = resolve_inbound(json!({
            "action": "forward",
            "targets": [
                "sip:legacy@example.com",
                {"trunk": "own-profile"},
                {"trunk": "no-profile"}
            ]
        }), trunks, Some("carrier-in")).await.unwrap();
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = plan.flow else {
            panic!("expected sequential targets");
        };
        let hints = |i: usize| attempts[i].hints.as_ref().expect("attempt hints");
        // A URI target dials with the arrival trunk's network path only.
        assert_eq!(hints(0).network_profile_id.as_deref(), Some("arrival"));
        assert_eq!(hints(0).external_ip.as_deref(), Some("198.51.100.1"));
        assert!(hints(0).allow_codecs.is_none());
        // A target trunk's own profile wins; without one the arrival's applies.
        assert_eq!(hints(1).network_profile_id.as_deref(), Some("dest"));
        assert_eq!(hints(2).network_profile_id.as_deref(), Some("arrival"));
        assert_eq!(outbound_trunk(&attempts[2]), "no-profile");

        // Without an arrival trunk, URI targets carry no hints.
        let internal = resolve_with_response(json!({
            "action": "forward", "targets": ["sip:legacy@example.com"]
        })).await.unwrap();
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = internal.flow else {
            panic!("expected sequential targets");
        };
        assert!(attempts[0].hints.is_none());
    }

    #[tokio::test]
    async fn test_http_router_rtp_config_beats_trunk_network_path() {
        let mut arrival = trunk("sip:192.0.2.9", None);
        arrival.profile = Some("arrival".to_string());
        arrival.external_ip = Some("198.51.100.1".to_string());
        let mut own = trunk("sip:10.0.0.2", None);
        own.profile = Some("dest".to_string());
        own.bind_ip = Some("10.0.0.99".to_string());
        let trunks = [
            ("carrier-in".to_string(), arrival),
            ("own-profile".to_string(), own),
        ]
        .into();
        let plan = resolve_inbound(json!({
            "action": "forward",
            "targets": ["sip:legacy@example.com", {"trunk": "own-profile"}],
            "rtp_config": {"bind_ip": "127.0.0.1", "external_ip": "203.0.113.7"}
        }), trunks, Some("carrier-in")).await.unwrap();
        assert_eq!(plan.media.bind_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(plan.media.external_ip.as_deref(), Some("203.0.113.7"));
        let DialplanFlow::Targets(DialStrategy::Sequential(attempts)) = plan.flow else {
            panic!("expected sequential targets");
        };
        for attempt in &attempts {
            let hints = attempt.hints.as_ref().expect("attempt hints");
            assert_eq!(hints.bind_ip.as_deref(), Some("127.0.0.1"));
            assert_eq!(hints.external_ip.as_deref(), Some("203.0.113.7"));
        }
        // Profiles still apply underneath (ports, SIP Contact).
        assert_eq!(attempts[0].hints.as_ref().unwrap().network_profile_id.as_deref(), Some("arrival"));
        assert_eq!(attempts[1].hints.as_ref().unwrap().network_profile_id.as_deref(), Some("dest"));
    }
}
