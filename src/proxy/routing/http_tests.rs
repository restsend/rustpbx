#[cfg(test)]
mod tests {
    use crate::call::{SipUser, TransactionCookie};
    use crate::config::{HttpRouterConfig, MediaProxyMode, RtpConfig};
    use crate::proxy::call::CallRouter;
    use crate::proxy::routing::http::HttpCallRouter;
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use tokio::net::TcpListener;

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

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(config, RtpConfig::default(), MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
            async fn preview_route(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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

        assert_eq!(dialplan.recording.enabled, true);
        assert_eq!(dialplan.max_call_duration.unwrap().as_secs(), 30);

        let payload = rx.recv().await.unwrap();
        assert_eq!(payload["call_id"], "test-call-id");
        assert_eq!(payload["from"], "sip:caller@example.com");
        assert_eq!(payload["to"], "sip:target@example.com");
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

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(config, RtpConfig::default(), MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
            async fn preview_route(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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
                assert!(err.extensions.is_none(), "extensions should be None when not provided in reject");
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

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(config, RtpConfig::default(), MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
            async fn preview_route(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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
        let exts = err.extensions.expect("extensions should be preserved on abort");
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

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(config, RtpConfig::default(), MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
            async fn preview_route(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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
        let exts = err.extensions.expect("extensions should be preserved on reject");
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

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(config, RtpConfig::default(), MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
            async fn preview_route(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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
        assert!(err.extensions.is_none(), "extensions should be None when not provided");
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

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpRouterConfig {
            url: format!("http://{}/route", addr),
            headers: None,
            fallback_to_static: false,
            timeout_ms: Some(1000),
        };

        let router = HttpCallRouter::new(config, RtpConfig::default(), MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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
            crate::config::MediaProxyMode::None
        );
        assert_eq!(dialplan.with_original_headers, true);

        let target = dialplan.first_target().unwrap();
        let header = target.headers.as_ref().unwrap().get(0).unwrap();
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
                    "media_proxy": "all"
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
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
            start_port: Some(10000),
            end_port: Some(20000),
            ..Default::default()
        };

        let router = HttpCallRouter::new(config, rtp_config, MediaProxyMode::None);

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsipstack::sip::Header::To("sip:target@example.com".try_into().unwrap()),
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
        impl crate::call::RouteInvite for DummyRouteInvite {
            async fn route_invite(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
                    rsipstack::dialog::invitation::InviteOption::default(),
                    None,
                ))
            }
            async fn preview_route(
                &self,
                _: rsipstack::dialog::invitation::InviteOption,
                _: &rsipstack::sip::Request,
                _: &crate::call::DialDirection,
                _: &TransactionCookie,
            ) -> anyhow::Result<crate::config::RouteResult> {
                Ok(crate::config::RouteResult::NotHandled(
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
            crate::config::MediaProxyMode::All
        );
        // Verify RTP config from server is applied
        assert_eq!(dialplan.media.external_ip.as_deref(), Some("203.0.113.1"));
        assert_eq!(dialplan.media.rtp_start_port, Some(10000));
        assert_eq!(dialplan.media.rtp_end_port, Some(20000));
    }
}
