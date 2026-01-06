#[cfg(test)]
mod tests {
    use crate::call::{SipUser, TransactionCookie};
    use crate::config::HttpRouterConfig;
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

        let router = HttpCallRouter::new(config);

        let request = rsip::Request {
            method: rsip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsip::Header::To("sip:target@example.com".try_into().unwrap()),
                rsip::Header::CallId("test-call-id".into()),
            ]
            .into(),
            version: rsip::Version::V2,
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
                _: &rsip::Request,
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
                _: &rsip::Request,
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
        assert_eq!(dialplan.call_timeout.as_secs(), 30);

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

        let router = HttpCallRouter::new(config);

        let request = rsip::Request {
            method: rsip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsip::Header::To("sip:target@example.com".try_into().unwrap()),
                rsip::Header::CallId("test-call-id".into()),
            ]
            .into(),
            version: rsip::Version::V2,
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
                _: &rsip::Request,
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
                _: &rsip::Request,
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
            Err((err, Some(status))) => {
                assert_eq!(status, rsip::StatusCode::Forbidden);
                assert!(err.to_string().contains("Forbidden by test"));
            }
            _ => panic!("Expected rejection"),
        }
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

        let router = HttpCallRouter::new(config);

        let request = rsip::Request {
            method: rsip::Method::Invite,
            uri: "sip:target@example.com".try_into().unwrap(),
            headers: vec![
                rsip::Header::From("sip:caller@example.com".try_into().unwrap()),
                rsip::Header::To("sip:target@example.com".try_into().unwrap()),
                rsip::Header::CallId("test-id".into()),
            ]
            .into(),
            version: rsip::Version::V2,
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
                _: &rsip::Request,
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
}
