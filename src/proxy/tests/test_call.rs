//! proxy/call.rs test suite (moved verbatim from the inline `mod tests`).
//! Repo convention: module sources live in `src/proxy/tests/test_*.rs`.

#![cfg(test)]

use crate::proxy::call::*;
use crate::proxy::ProxyModule;
use rsipstack::sip::prelude::HeadersExt;
use tokio_util::sync::CancellationToken;
use audio_codec::CodecType;
use anyhow::Result;
use crate::proxy::server::SipServerRef;
use crate::proxy::active_call_registry::ActiveProxyCallStatus;
use std::sync::Arc;
use crate::call::DialStrategy;
use crate::config::{ProxyConfig, RecordingPolicy};
use crate::proxy::routing::{RouteRule, TrunkConfig};
use crate::call::{Dialplan, DialplanFlow, RoutingState};
use crate::call::{CalleeOfflineMarker, TrunkContext};
use crate::call::Location;
use crate::call::{DialDirection, RouteInvite, SipUser, TransactionCookie};
use crate::config::RouteResult;
use crate::proxy::tests::common::{create_test_server, create_test_server_with_config};
use crate::proxy::proxy_call::sip_session::SipSession;
use async_trait::async_trait;
use rsipstack::dialog::invitation::InviteOption;

fn make_loc() -> Vec<Location> {
    vec![Location {
        aor: rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "test".to_string(),
                password: None,
            }),
            host_with_port: rsipstack::sip::HostWithPort {
                host: rsipstack::sip::Host::Domain("example.com".to_string().into()),
                port: None,
            },
            params: vec![],
            headers: vec![],
        },
        ..Default::default()
    }]
}

#[tokio::test]
async fn peer_ring_timeout_message_reaches_only_a_local_registered_phone() {
    use crate::proxy::tests::common::{create_test_request, create_transaction};
    use rsipstack::sip::{Method, SipMessage};
    use rsipstack::transport::{SipAddr, channel::ChannelConnection, connection::TransportEvent};
    for (trusted, local_home) in [(true, true), (false, true), (true, false)] {
        let (mut server, config) = create_test_server().await;
        let mut local: SipAddr = "sip:127.0.0.1:5060"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        let mut remote: SipAddr = "sip:192.0.2.44:5060"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        let mut destination: SipAddr = "sip:127.0.0.1:5070"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        local.r#type = Some(rsipstack::sip::Transport::Udp);
        remote.r#type = Some(rsipstack::sip::Transport::Udp);
        destination.r#type = Some(rsipstack::sip::Transport::Udp);
        let inner = Arc::get_mut(&mut server).unwrap();
        inner.cluster_peer_ips = if trusted {
            vec!["192.0.2.44".parse().unwrap()]
        } else {
            vec![]
        };
        inner.cluster_self_addr = Some(local.clone());
        for addr in server.endpoint.inner.transport_layer.get_addrs() {
            server.endpoint.inner.transport_layer.del_transport(&addr);
        }
        let (_incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outgoing, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel();
        let transport = ChannelConnection::create_connection(
            incoming_rx, outgoing, local.clone(), None,
        ).await.unwrap();
        server.endpoint.inner.transport_layer.add_transport(transport.into());
        let canonical: rsipstack::sip::Uri = "sip:agent@127.0.0.1:5060".parse().unwrap();
        let contact: rsipstack::sip::Uri = "sip:agent@phone.invalid".parse().unwrap();
        server.locator.register("agent", Some("127.0.0.1"), Location {
            aor: contact.clone(),
            registered_aor: Some(canonical),
            home_proxy: Some(if local_home { local.clone() } else { remote.clone() }),
            destination: Some(destination),
            expires: 3600,
            last_modified: Some(std::time::Instant::now()),
            ..Default::default()
        }).await.unwrap();
        let mut request = create_test_request(Method::Message, "caller", None, "example.test", None);
        request.uri = "sip:agent@127.0.0.1:5060".parse().unwrap();
        request.headers.push(rsipstack::sip::Header::ContentType(crate::call::sip::CALL_LEG_EVENT_CT.into()));
        request.body = br#"{"event":"ring_timeout","call_id":"upstream-call","offer":"urn:uuid:offer-a"}"#.to_vec();
        let body = request.body.clone();
        let (mut transaction, _) = create_transaction(request).await;
        let (_source_tx, source_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        transaction.connection = Some(ChannelConnection::create_connection(
            source_rx, response_tx, remote, None,
        ).await.unwrap().into());
        let module = CallModule::new(config, server);
        module.handle_message(&mut transaction, &TransactionCookie::default())
            .await.unwrap();
        let Some(TransportEvent::Incoming(SipMessage::Response(response), _, _)) =
            response_rx.recv().await
        else { panic!("the peer transaction must receive a response"); };
        if trusted && local_home {
            assert_eq!(response.status_code, rsipstack::sip::StatusCode::OK);
            let received = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if let Some(TransportEvent::Incoming(SipMessage::Request(request), _, _)) =
                        outgoing_rx.recv().await
                    {
                        if request.method == Method::Message {
                            break request;
                        }
                    }
                }
            }).await.expect("the home proxy must deliver expiry to its registered phone");
            assert_eq!(received.uri, contact);
            assert_eq!(received.body, body);
        } else {
            assert!(outgoing_rx.try_recv().is_err(),
                "untrusted or remote-home events must not relay");
            assert_eq!(
                response.status_code,
                if trusted {
                    rsipstack::sip::StatusCode::NotFound
                } else {
                    rsipstack::sip::StatusCode::Forbidden
                },
            );
        }
    }
}

#[tokio::test]
async fn peer_admin_cmd_message_reaches_only_the_home_registered_phone() {
    use crate::proxy::tests::common::{create_test_request, create_transaction};
    use std::io::{Read, Seek, SeekFrom};
    use tracing::instrument::WithSubscriber;
    use rsipstack::sip::{Method, SipMessage};
    use rsipstack::transport::{SipAddr, channel::ChannelConnection, connection::TransportEvent};
    for (trusted, local_home) in [(true, true), (false, true), (true, false)] {
        let (mut server, config) = create_test_server().await;
        let mut local: SipAddr = "sip:127.0.0.1:5060"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        let mut remote: SipAddr = "sip:192.0.2.44:5060"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        let mut destination: SipAddr = "sip:127.0.0.1:5070"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        local.r#type = Some(rsipstack::sip::Transport::Udp);
        remote.r#type = Some(rsipstack::sip::Transport::Udp);
        destination.r#type = Some(rsipstack::sip::Transport::Udp);
        let inner = Arc::get_mut(&mut server).unwrap();
        inner.cluster_peer_ips = if trusted {
            vec!["192.0.2.44".parse().unwrap()]
        } else {
            vec![]
        };
        inner.cluster_self_addr = Some(local.clone());
        for addr in server.endpoint.inner.transport_layer.get_addrs() {
            server.endpoint.inner.transport_layer.del_transport(&addr);
        }
        let (_incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outgoing, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel();
        let transport = ChannelConnection::create_connection(
            incoming_rx, outgoing, local.clone(), None,
        ).await.unwrap();
        server.endpoint.inner.transport_layer.add_transport(transport.into());
        let canonical: rsipstack::sip::Uri = "sip:agent@127.0.0.1:5060".parse().unwrap();
        let contact: rsipstack::sip::Uri = "sip:agent@phone.invalid".parse().unwrap();
        server.locator.register("agent", Some("127.0.0.1"), Location {
            aor: contact.clone(),
            registered_aor: Some(canonical),
            home_proxy: Some(if local_home { local.clone() } else { remote.clone() }),
            destination: Some(destination),
            expires: 3600,
            last_modified: Some(std::time::Instant::now()),
            ..Default::default()
        }).await.unwrap();
        let mut request = create_test_request(Method::Message, "caller", None, "example.test", None);
        request.uri = "sip:agent@127.0.0.1:5060".parse().unwrap();
        request.headers.push(rsipstack::sip::Header::ContentType(
            crate::call::sip::ADMIN_CMD_RELAY_CT.into()));
        request.body = br#"{"v":1,"cmd":"shot","trans_id":"tx-1","agent":"agent","offer":"v=0","token":"admin-secret-regression"}"#.to_vec();
        let body = request.body.clone();
        let (mut transaction, _) = create_transaction(request).await;
        let (_source_tx, source_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        transaction.connection = Some(ChannelConnection::create_connection(
            source_rx, response_tx, remote, None,
        ).await.unwrap().into());
        let module = CallModule::new(config, server);
        let mut log_file = tempfile::tempfile().unwrap();
        let writer = log_file.try_clone().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.try_clone().unwrap())
            .finish();
        module.handle_message(&mut transaction, &TransactionCookie::default())
            .with_subscriber(subscriber)
            .await.unwrap();
        log_file.seek(SeekFrom::Start(0)).unwrap();
        let mut logs = String::new();
        log_file.read_to_string(&mut logs).unwrap();
        assert!(logs.contains("Received SIP MESSAGE"));
        assert!(
            !logs.contains("admin-secret-regression"),
            "admin credentials must not be logged"
        );
        let Some(TransportEvent::Incoming(SipMessage::Response(response), _, _)) =
            response_rx.recv().await
        else { panic!("the peer transaction must receive a response"); };
        if trusted && local_home {
            assert_eq!(response.status_code, rsipstack::sip::StatusCode::OK);
            let received = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if let Some(TransportEvent::Incoming(SipMessage::Request(request), _, _)) =
                        outgoing_rx.recv().await
                    {
                        if request.method == Method::Message {
                            break request;
                        }
                    }
                }
            }).await.expect("the home proxy must deliver the admin cmd to its registered phone");
            assert_eq!(received.uri, contact);
            assert_eq!(received.body, body);
            // 终端投递用客户端信封 CT（不再带 RELAY 标记）。
            assert!(received.headers.iter().any(|h| matches!(h,
                rsipstack::sip::Header::ContentType(ct)
                    if ct.value() == crate::call::sip::ADMIN_CMD_CT)));
        } else {
            assert!(outgoing_rx.try_recv().is_err(),
                "untrusted or remote-home admin cmds must not deliver");
            assert_eq!(
                response.status_code,
                if trusted {
                    rsipstack::sip::StatusCode::NotFound
                } else {
                    rsipstack::sip::StatusCode::Forbidden
                },
            );
        }
    }
}

/// AMI 创建入口的跨节点分支：home 在对端 → RELAY CT pager 到对端
/// destination（不是终端 CT）；home 在本节点 → 终端 CT 直达坐席。
#[tokio::test]
async fn admin_cmd_entry_relays_only_when_home_is_remote() {
    use crate::proxy::tests::common::{
        create_register_request, create_test_server, create_transaction,
    };
    use rsipstack::sip::{Method, SipMessage};
    use rsipstack::transport::{SipAddr, channel::ChannelConnection, connection::TransportEvent};
    for (clustered, home_is_remote, want_ct) in [
        (true, true, crate::call::sip::ADMIN_CMD_RELAY_CT),
        (true, false, crate::call::sip::ADMIN_CMD_CT),
        (false, false, crate::call::sip::ADMIN_CMD_CT),
    ] {
        let (mut server, config) = create_test_server().await;
        let mut local: SipAddr = "sip:127.0.0.1:5060"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        let mut remote: SipAddr = "sip:192.0.2.44:5060"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        let mut destination: SipAddr = "sip:127.0.0.1:5070"
            .parse::<rsipstack::sip::Uri>().unwrap().try_into().unwrap();
        local.r#type = Some(rsipstack::sip::Transport::Udp);
        remote.r#type = Some(rsipstack::sip::Transport::Udp);
        destination.r#type = Some(rsipstack::sip::Transport::Udp);
        let inner = Arc::get_mut(&mut server).unwrap();
        inner.cluster_self_addr = clustered.then(|| local.clone());
        for addr in server.endpoint.inner.transport_layer.get_addrs() {
            server.endpoint.inner.transport_layer.del_transport(&addr);
        }
        let (_incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outgoing, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel();
        let transport = ChannelConnection::create_connection(
            incoming_rx, outgoing, local.clone(), None,
        ).await.unwrap();
        server.endpoint.inner.transport_layer.add_transport(transport.into());
        let canonical: rsipstack::sip::Uri = "sip:agent@127.0.0.1:5060".parse().unwrap();
        let contact: rsipstack::sip::Uri = "sip:agent@phone.invalid".parse().unwrap();
        if !clustered {
            // Exercise the registrar's real single-node home_proxy fallback.
            let mut request = create_register_request("agent", "127.0.0.1", Some(3600));
            request.headers.unique_push(rsipstack::sip::Header::Contact(
                "<sip:agent@phone.invalid>".into(),
            ));
            let (mut transaction, _) = create_transaction(request).await;
            crate::proxy::registrar::RegistrarModule::new(server.clone(), config)
                .on_transaction_begin(
                    CancellationToken::new(),
                    &mut transaction,
                    TransactionCookie::default(),
                )
                .await.unwrap();
            let locations = server.locator.lookup(&canonical).await.unwrap();
            assert_eq!(locations.len(), 1);
            assert!(locations[0].home_proxy.is_some());
        } else {
            server.locator.register("agent", Some("127.0.0.1"), Location {
                aor: contact.clone(),
                registered_aor: Some(canonical),
                home_proxy: Some(if home_is_remote { remote.clone() } else { local.clone() }),
                destination: Some(destination),
                expires: 3600,
                last_modified: Some(std::time::Instant::now()),
                ..Default::default()
            }).await.unwrap();
        }
        let body = br#"{"v":1,"cmd":"log","trans_id":"tx-2","agent":"agent","days":1}"#.to_vec();
        // 短等待窗口：本 harness 无人应答 MESSAGE——投递即证路由正确，
        // 结果应为 NoAnswer（而非 NotRegistered）。
        let outcome = crate::call::sip::deliver_admin_cmd_with_wait(
            &server,
            "agent",
            &body,
            std::time::Duration::from_millis(300),
        )
        .await
        .expect("delivery attempt must not error");
        assert!(
            matches!(outcome, crate::call::sip::AdminCmdOutcome::NoAnswer),
            "registered agent must be found and paged, got {outcome:?}"
        );
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(TransportEvent::Incoming(SipMessage::Request(request), _, _)) =
                    outgoing_rx.recv().await
                {
                    if request.method == Method::Message {
                        break request;
                    }
                }
            }
        }).await.expect("a pager MESSAGE must go out");
        assert_eq!(received.uri, contact);
        assert_eq!(received.body, body);
        assert!(received.headers.iter().any(|h| matches!(h,
            rsipstack::sip::Header::ContentType(ct) if ct.value() == want_ct)),
            "expected CT {want_ct}");
    }
}

struct NotHandledRouteInvite;

#[async_trait]
impl RouteInvite for NotHandledRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        _origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        Ok(RouteResult::NotHandled(option, None))
    }
}

#[tokio::test]
async fn chained_route_invite_falls_back_on_not_handled() {
    let chained = ChainedRouteInvite::new(
        Box::new(NotHandledRouteInvite),
        Box::new(ApplicationRouteInvite { headers: None }),
    );
    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: rsipstack::sip::Uri::try_from("sip:bob@example.com").unwrap(),
        version: rsipstack::sip::Version::V2,
        headers: vec![
            rsipstack::sip::Header::From("<sip:alice@example.com>".try_into().unwrap()),
            rsipstack::sip::Header::To("<sip:bob@example.com>".into()),
            rsipstack::sip::Header::CallId("chained-test".into()),
            rsipstack::sip::Header::CSeq("1 INVITE".try_into().unwrap()),
        ]
        .into(),
        body: Vec::new(),
    };
    let cookie = TransactionCookie::default();
    let result = chained
        .route_invite(
            InviteOption::default(),
            &request,
            &DialDirection::Outbound,
            &cookie,
        )
        .await
        .expect("chained fallback");
    match result {
        RouteResult::Application { app_name, .. } => assert_eq!(app_name, "ivr"),
        _ => panic!("expected fallback application route"),
    }
}

struct ApplicationRouteInvite {
    headers: Option<Vec<rsipstack::sip::Header>>,
}

#[async_trait]
impl RouteInvite for ApplicationRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        _origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let mut opt = option;
        opt.headers = self.headers.clone();
        Ok(RouteResult::Application {
            option: opt,
            app_name: "ivr".to_string(),
            app_params: None,
            auto_answer: true,
            hints: None,
        })
    }
}

struct RoutePointApplicationRouteInvite;

#[async_trait]
impl RouteInvite for RoutePointApplicationRouteInvite {
    async fn route_invite(
        &self,
        mut option: InviteOption,
        origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        assert_eq!(option.callee.user(), Some("39230"));
        assert_eq!(origin.uri.user(), Some("39230"));
        assert_eq!(
            origin
                .to_header()
                .expect("route-point To header")
                .uri()
                .expect("route-point To URI")
                .user(),
            Some("39230")
        );
        option.headers = Some(vec![rsipstack::sip::Header::Other(
            "X-Business-Key".into(),
            "driver-spring-festival".into(),
        )]);
        Ok(RouteResult::Application {
            option,
            app_name: "step_ivr".to_string(),
            app_params: Some(serde_json::json!({
                "businessKey": "driver-spring-festival"
            })),
            auto_answer: true,
            hints: None,
        })
    }
}

struct RoutePointNotHandledRouteInvite;

#[async_trait]
impl RouteInvite for RoutePointNotHandledRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        _origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let mut hints = crate::config::DialplanHints::default();
        hints
            .concurrency_holds
            .push(crate::call::policy::ConcurrencyHold {
                policy_id: "route-point-policy".to_string(),
                scope: "caller".to_string(),
                scope_value: "bp".to_string(),
            });
        Ok(RouteResult::NotHandled(option, Some(hints)))
    }
}

struct RewrittenForwardRouteInvite;

#[async_trait]
impl RouteInvite for RewrittenForwardRouteInvite {
    async fn route_invite(
        &self,
        mut option: InviteOption,
        _origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        option.caller =
            rsipstack::sip::Uri::try_from("sip:rewritten-caller@source.example.com")?;
        option.callee = rsipstack::sip::Uri::try_from("sip:001234@carrier.example.com:5060")?;
        Ok(RouteResult::Forward(vec![crate::config::RouteTarget::new(option, Some(Default::default()))]))
    }
}

#[tokio::test]
async fn default_resolve_preserves_retry_invites_hints_and_policies() {
    struct RetryRouter(u64);
    #[async_trait]
    impl RouteInvite for RetryRouter {
        async fn route_invite(
            &self, _option: InviteOption, _origin: &rsipstack::sip::Request,
            _direction: &DialDirection, _cookie: &TransactionCookie,
        ) -> Result<RouteResult> {
            Ok(RouteResult::Forward((0..self.0).map(|index| crate::config::RouteTarget {
                option: InviteOption {
                    caller: format!("sip:caller{index}@source.example.com").try_into().unwrap(),
                    callee: format!("sip:target{index}@carrier.example.com").try_into().unwrap(),
                    ..Default::default()
                },
                hints: Some(crate::config::DialplanHints {
                    max_ring_time: Some(std::time::Duration::from_secs(12 + index)),
                    ..Default::default()
                }),
                // The last target has no policy, which never advances.
                retry_policy: (index == 0).then(|| crate::proxy::routing::RetryPolicy {
                    status_codes: vec![503, 408],
                }),
            }).collect()))
        }
    }
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);
    let request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite, "original-caller", None, "rustpbx.com", None,
    );
    let caller = SipUser { username: "original-caller".into(), realm: Some("rustpbx.com".into()), ..Default::default() };
    let plan = module.default_resolve(&request, Box::new(RetryRouter(2)), &caller, &TransactionCookie::default()).await.unwrap();
    let crate::call::DialplanFlow::Targets(DialStrategy::Sequential(targets)) = &plan.flow else { panic!("expected sequential plan"); };
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].location.aor.user(), Some("target0"));
    assert_eq!(targets[1].location.aor.user(), Some("target1"));
    let attempts = targets;
    assert_eq!(attempts[0].caller.as_ref().unwrap().user(), Some("caller0"));
    assert_eq!(attempts[1].caller.as_ref().unwrap().user(), Some("caller1"));
    for (index, attempt) in attempts.iter().enumerate() {
        assert_eq!(attempt.hints.as_ref().unwrap().max_ring_time, Some(std::time::Duration::from_secs(12 + index as u64)));
    }
    let policy = attempts[0].retry_policy.as_ref().unwrap();
    assert_eq!(policy.status_codes, vec![503, 408]);
    assert!(attempts[1].retry_policy.as_ref().unwrap().status_codes.is_empty());

    let empty = module.default_resolve(
        &request, Box::new(RetryRouter(0)), &caller, &TransactionCookie::default(),
    ).await.unwrap();
    assert!(empty.is_empty(), "an empty Forward must not fall back to direct dialing");
}

#[tokio::test]
async fn default_resolve_fixes_call_settings_from_the_first_of_several_targets() {
    struct TwoTrunks;
    #[async_trait]
    impl RouteInvite for TwoTrunks {
        async fn route_invite(
            &self, option: InviteOption, _origin: &rsipstack::sip::Request,
            _direction: &DialDirection, _cookie: &TransactionCookie,
        ) -> Result<RouteResult> {
            let target = |user: &str, hints: crate::config::DialplanHints| {
                let mut option = option.clone();
                option.callee = format!("sip:{user}@carrier.example.com").try_into().unwrap();
                crate::config::RouteTarget::new(option, Some(hints))
            };
            Ok(RouteResult::Forward(vec![
                target("a", crate::config::DialplanHints {
                    recording: Some(RecordingPolicy {
                        enabled: Some(true),
                        path: Some("/tmp/rustpbx-first-target".into()),
                        ..Default::default()
                    }),
                    max_ring_time: Some(std::time::Duration::from_secs(40)),
                    ..Default::default()
                }),
                target("b", crate::config::DialplanHints {
                    enable_recording: Some(false),
                    ..Default::default()
                }),
            ]))
        }
    }
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);
    let request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite, "alice", None, "rustpbx.com", None,
    );
    let caller = SipUser { username: "alice".into(), realm: Some("rustpbx.com".into()), ..Default::default() };
    let plan = module
        .default_resolve(&request, Box::new(TwoTrunks), &caller, &TransactionCookie::default())
        .await
        .unwrap();
    assert!(plan.recording.enabled, "the first target's recording applies to the call");
    assert_eq!(
        plan.recording_policy.as_ref().and_then(|policy| policy.path.as_deref()),
        Some("/tmp/rustpbx-first-target")
    );
    assert_eq!(plan.max_ring_time, None, "a trunk's ring time limits its own attempt, not the call");
    assert!(plan.extensions.get::<crate::call::OutboundTrunkContext>().is_none(), "other hints stay per attempt");
}

struct RecordingHintsRouteInvite {
    recording: Option<RecordingPolicy>,
    enable_recording: Option<bool>,
}

#[async_trait]
impl RouteInvite for RecordingHintsRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        _origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let mut hints = crate::config::DialplanHints::default();
        hints.recording = self.recording.clone();
        hints.enable_recording = self.enable_recording;
        Ok(RouteResult::Forward(vec![crate::config::RouteTarget::new(option, Some(hints))]))
    }
}

/// Emits a per-trunk ICE-lite override, as `merge_trunk_media_hints`
/// would for `[trunk.<name>] ice_lite`.
struct IceLiteHintsRouteInvite {
    ice_lite: Option<bool>,
}

#[async_trait]
impl RouteInvite for IceLiteHintsRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        _origin: &rsipstack::sip::Request,
        _direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let mut hints = crate::config::DialplanHints::default();
        hints.ice_lite = self.ice_lite;
        Ok(RouteResult::Forward(vec![crate::config::RouteTarget::new(option, Some(hints))]))
    }
}

fn replace_to_header(request: &mut rsipstack::sip::Request, to_uri: rsipstack::sip::Uri) {
    request
        .headers
        .retain(|header| !matches!(header, rsipstack::sip::Header::To(_)));
    request.headers.push(
        rsipstack::sip::typed::To {
            display_name: None,
            uri: to_uri,
            params: vec![],
        }
        .into(),
    );
}

#[test]
fn loop_guard_same_realm_online_user_passes() {
    let result = resolve_unhandled_targets(true, false, make_loc(), true);
    assert!(
        result.is_ok(),
        "same-realm online user should not be rejected"
    );
}

#[test]
fn loop_guard_same_realm_offline_returns_empty_targets() {
    let result = resolve_unhandled_targets(true, true, make_loc(), true);
    assert!(result.is_ok(), "offline same-realm should not error");
    match result.unwrap() {
        DialStrategy::Sequential(locs) => {
            assert!(
                locs.is_empty(),
                "offline same-realm should return empty targets"
            );
        }
        _ => panic!("expected Sequential strategy"),
    }
}

#[test]
fn loop_guard_external_callee_falls_through_to_locs() {
    // external callee with NotHandled should fall through to locs so that
    // dialplan inspectors (e.g. zhongan inviter) can rewrite the target.
    // A single location uses the sequential dialer (upstream change
    // "sequential for 1 location").
    let result = resolve_unhandled_targets(false, false, make_loc(), true);
    assert!(
        result.is_ok(),
        "external callee should fall through to Sequential(locs)"
    );
    let strategy = result.unwrap();
    match strategy {
        DialStrategy::Sequential(locs) => {
            assert!(!locs.is_empty(), "locs should contain the callee URI");
        }
        _ => panic!("expected Sequential strategy for a single target"),
    }
}

#[test]
fn loop_guard_external_callee_falls_through_offline_flag_ignored() {
    // external callee — internal_lookup_empty is set but irrelevant;
    // should still fall through because the offline check only applies to same-realm.
    let result = resolve_unhandled_targets(false, true, make_loc(), true);
    assert!(
        result.is_ok(),
        "external callee should fall through, offline flag is ignored"
    );
}

#[tokio::test]
async fn default_resolve_preserves_rewritten_route_uris_in_dialplan() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);
    let request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "original-caller",
        None,
        "rustpbx.com",
        None,
    );
    let caller = SipUser {
        username: "original-caller".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RewrittenForwardRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("rewritten route should produce a dialplan");

    assert_eq!(
        dialplan.caller.as_ref().map(ToString::to_string).as_deref(),
        Some("sip:rewritten-caller@source.example.com")
    );
    assert_eq!(
        dialplan
            .first_target()
            .map(|target| target.aor.to_string())
            .as_deref(),
        Some("sip:001234@carrier.example.com:5060")
    );
}

/// Per-extension ICE-lite detection: a caller whose registration declared
/// `;+sip.ice` flips `dialplan.media.ice_lite`, which the session applies
/// to its plain-RTP legs (a=ice-lite answers). Default stays off.
#[tokio::test]
async fn default_resolve_caller_ice_lite_sets_media_flag() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);
    let request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "original-caller",
        None,
        "rustpbx.com",
        None,
    );
    let caller = SipUser {
        username: "original-caller".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ice_lite: true,
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RewrittenForwardRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("dialplan");
    assert!(
        dialplan.media.ice_lite,
        "caller with ;+sip.ice must enable ice_lite on the session media"
    );

    let plain_caller = SipUser {
        username: "original-caller".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };
    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RewrittenForwardRouteInvite),
            &plain_caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("dialplan");
    assert!(
        !dialplan.media.ice_lite,
        "callers without the detection must keep ice_lite off"
    );
}

/// Trunk override wins over the per-extension detection: an explicit
/// `ice_lite = false` on the trunk downgrades an ice-detected caller.
#[tokio::test]
async fn default_resolve_trunk_ice_lite_hint_overrides_caller_detection() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);
    let request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "original-caller",
        None,
        "rustpbx.com",
        None,
    );
    let caller = SipUser {
        username: "original-caller".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ice_lite: true,
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(IceLiteHintsRouteInvite {
                ice_lite: Some(false),
            }),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("dialplan");
    assert!(
        !dialplan.media.ice_lite,
        "explicit trunk override must beat the caller detection"
    );

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(IceLiteHintsRouteInvite {
                ice_lite: Some(true),
            }),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("dialplan");
    assert!(dialplan.media.ice_lite);
}

// ---------------------------------------------------------------------------
// New tests: resolve_unhandled_targets + default_resolve + CalleeOfflineMarker
// ---------------------------------------------------------------------------

#[test]
fn resolve_unhandled_targets_same_realm_offline_returns_empty() {
    // same-realm + offline (internal_lookup_empty=true) should return Ok with empty targets,
    // not 480 — the rejection is deferred to build_dialplan.
    let result = resolve_unhandled_targets(true, true, make_loc(), true);
    assert!(result.is_ok());
    match result.unwrap() {
        DialStrategy::Sequential(locs) => assert!(locs.is_empty()),
        _ => panic!("expected Sequential"),
    }
}

#[test]
fn resolve_unhandled_targets_external_callee_retains_locs() {
    // A single external target is retained but uses the simple dialer.
    let locs = make_loc();
    let result = resolve_unhandled_targets(false, false, locs.clone(), true);
    let strategy = result.unwrap();
    match strategy {
        DialStrategy::Sequential(l) => assert_eq!(l.len(), locs.len()),
        _ => panic!("expected Sequential"),
    }
}

#[test]
fn resolve_unhandled_targets_single_registration_does_not_fork() {
    let locs = make_loc();
    let result = resolve_unhandled_targets(true, false, locs.clone(), true);
    let strategy = result.unwrap();
    match strategy {
        DialStrategy::Sequential(l) => assert_eq!(l.len(), 1),
        _ => panic!("one registration must use the sequential dialer"),
    }
}

#[test]
fn resolve_unhandled_targets_parallel_forks_all_locs() {
    // parallel_fork=true: every registered binding is returned so the
    // session can ring them all at once (first to answer wins).
    let mut locs = make_loc();
    locs.push(Location {
        aor: rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "test2".to_string(),
                password: None,
            }),
            host_with_port: rsipstack::sip::HostWithPort {
                host: rsipstack::sip::Host::Domain("example.com".to_string().into()),
                port: None,
            },
            params: vec![],
            headers: vec![],
        },
        ..Default::default()
    });
    let result = resolve_unhandled_targets(false, false, locs.clone(), true);
    let strategy = result.unwrap();
    match strategy {
        DialStrategy::Parallel(l) => assert_eq!(l.len(), locs.len()),
        _ => panic!("expected Parallel strategy"),
    }
}

#[test]
fn resolve_unhandled_targets_last_alive_dials_only_newest() {
    // parallel_fork=false: legacy "last alive" behavior — only the most
    // recently registered device (first in the recency-sorted list) is dialed.
    let mut locs = make_loc();
    locs.push(Location {
        aor: rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "test2".to_string(),
                password: None,
            }),
            host_with_port: rsipstack::sip::HostWithPort {
                host: rsipstack::sip::Host::Domain("example.com".to_string().into()),
                port: None,
            },
            params: vec![],
            headers: vec![],
        },
        ..Default::default()
    });
    let result = resolve_unhandled_targets(false, false, locs, false);
    let strategy = result.unwrap();
    match strategy {
        DialStrategy::Sequential(l) => {
            assert_eq!(l.len(), 1, "last-alive mode must dial exactly one device");
        }
        _ => panic!("expected Sequential strategy"),
    }
}

#[tokio::test]
async fn default_resolve_single_forward_keeps_registered_contacts_and_hints() {
    let (server, config) = create_test_server_with_config(ProxyConfig {
        parallel_fork: true,
        ..Default::default()
    }).await;
    for port in [5071, 5072] {
        server.locator.register("alice", Some("rustpbx.com"), Location {
            aor: format!("sip:alice@127.0.0.1:{port}").parse().unwrap(),
            expires: 3600,
            ..Default::default()
        }).await.unwrap();
    }
    let module = CallModule::new(config, server);
    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite, "bp", None, "rustpbx.com", None,
    );
    request.uri = "sip:alice@rustpbx.com".parse().unwrap();
    replace_to_header(&mut request, "sip:alice@rustpbx.com".parse().unwrap());
    let caller = SipUser {
        username: "bp".into(), realm: Some("rustpbx.com".into()), ..Default::default()
    };
    let plan = module.default_resolve(
        &request, Box::new(IceLiteHintsRouteInvite { ice_lite: Some(true) }),
        &caller, &TransactionCookie::default(),
    ).await.unwrap();
    let crate::call::DialplanFlow::Targets(DialStrategy::Parallel(contacts)) = &plan.flow else {
        panic!("one logical route must still ring its registered contacts in parallel");
    };
    assert_eq!(contacts.len(), 2);
    let mut uris = contacts.iter().map(|contact| contact.aor.to_string()).collect::<Vec<_>>();
    uris.sort();
    assert_eq!(uris, vec!["sip:alice@127.0.0.1:5071", "sip:alice@127.0.0.1:5072"]);
    assert!(plan.media.ice_lite, "single-target route hints must still apply");
}

#[tokio::test]
async fn default_resolve_sets_offline_marker_for_empty_locator() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:nobody@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:nobody@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("offline user should not error at resolve time");

    assert!(dialplan.is_empty(), "targets should be empty");
    assert!(
        dialplan.extensions.get::<CalleeOfflineMarker>().is_some(),
        "offline marker should be set"
    );
}

#[tokio::test]
async fn default_resolve_home_hop_does_not_fork_back_to_peer() {
    let (mut server, config) = create_test_server_with_config(ProxyConfig {
        realms: Some(vec!["10.0.0.1".into()]),
        ..Default::default()
    }).await;
    let local = rsipstack::transport::SipAddr::try_from(
        rsipstack::sip::Uri::try_from("sip:10.0.0.1:5060").unwrap()).unwrap();
    Arc::get_mut(&mut server).unwrap().cluster_self_addr = Some(local.clone());
    for (contact, home) in [("local", "10.0.0.1"), ("peer", "10.0.0.2")] {
        server.locator.register("alice", Some("10.0.0.1"), Location {
            aor: format!("sip:{contact}@device.invalid;transport=ws").parse().unwrap(),
            registered_aor: Some("sip:alice@10.0.0.1:5060".parse().unwrap()),
            home_proxy: Some(rsipstack::transport::SipAddr::try_from(
                rsipstack::sip::Uri::try_from(format!("sip:{home}:5060")).unwrap()).unwrap()),
            destination: Some(rsipstack::transport::SipAddr::try_from(rsipstack::sip::Uri::try_from(
                format!("sip:192.0.2.1:{}", if contact == "local" { 7001 } else { 7002 })).unwrap()).unwrap()),
            expires: 3600,
            ..Default::default()
        }).await.unwrap();
    }
    let module = CallModule::new(config, server);
    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite, "bp", None, "rustpbx.com", None,
    );
    request.uri = "sip:alice@10.0.0.1:5060".parse().unwrap();
    let caller = SipUser {
        username: "bp".into(), realm: Some("rustpbx.com".into()), ..Default::default()
    };
    let dialplan = module.default_resolve(&request, Box::new(NotHandledRouteInvite), &caller,
        &TransactionCookie::default()).await.unwrap();
    let targets = dialplan.get_all_targets().unwrap();
    assert_eq!(targets.len(), 1, "a home hop must not start another cluster fork");
    assert_eq!(targets[0].home_proxy.as_ref(), Some(&local));
    // A canonical address still rings contacts across the cluster.
    request.uri = "sip:alice@rustpbx.com".parse().unwrap();
    let dialplan = module.default_resolve(&request, Box::new(NotHandledRouteInvite), &caller,
        &TransactionCookie::default()).await.unwrap();
    assert_eq!(dialplan.get_all_targets().unwrap().len(), 2);
}

#[tokio::test]
async fn default_resolve_does_not_set_marker_for_online_user() {
    let (server, config) = create_test_server().await;

    // Register alice in the locator so she's online (BEFORE creating module)
    let _ = server
        .locator
        .register(
            "alice",
            Some("rustpbx.com"),
            Location {
                aor: rsipstack::sip::Uri::try_from("sip:alice@rustpbx.com").unwrap(),
                expires: 3600,
                destination: Some(rsipstack::transport::SipAddr {
                    r#type: Some(rsipstack::sip::Transport::Udp),
                    addr: rsipstack::sip::HostWithPort {
                        host: "10.0.0.1".parse().unwrap(),
                        port: Some(5060.into()),
                    },
                }),
                ..Default::default()
            },
        )
        .await;

    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:alice@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:alice@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("online user should succeed");

    assert!(!dialplan.is_empty(), "online user should have targets");
    assert!(
        dialplan.extensions.get::<CalleeOfflineMarker>().is_none(),
        "online user should not have offline marker"
    );
}

#[tokio::test]
async fn default_resolve_does_not_set_marker_for_external_callee() {
    // external callee — even if locator returns empty, marker should not be set
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri =
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("external destination should succeed");

    assert!(!dialplan.is_empty(), "external callee should have targets");
    assert!(
        dialplan.extensions.get::<CalleeOfflineMarker>().is_none(),
        "external callee should not have offline marker"
    );
}

#[tokio::test]
async fn default_resolve_wholesale_trunk_gets_marker_and_empty_targets() {
    // Wholesale trunks: same-realm, locator empty, with TrunkContext.
    // resolve_unhandled_targets returns empty targets (not 480), and offline marker is set.
    // This is correct — build_dialplan will let inspectors fill targets later.
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri =
        rsipstack::sip::Uri::try_from("sip:+862161952290@rustpbx.com;transport=UDP").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:+862161952290@rustpbx.com").unwrap(),
    );

    // Caller from trunk — realm matches server so callee_is_same_realm=true
    let caller = SipUser {
        username: "+8617301791502".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let cookie = TransactionCookie::default();
    cookie.insert_extension(TrunkContext {
        id: Some(1),
        name: "wholesale-trunk".to_string(),
        did_numbers: vec![],
    });

    let dialplan = module
        .default_resolve(&request, Box::new(NotHandledRouteInvite), &caller, &cookie)
        .await
        .expect("wholesale trunk should not error at resolve time");

    assert!(
        dialplan.is_empty(),
        "wholesale locator empty => empty targets"
    );
    assert!(
        dialplan.extensions.get::<CalleeOfflineMarker>().is_some(),
        "offline marker should be set for same-realm locator-empty"
    );
}

#[tokio::test]
async fn default_resolve_applies_source_trunk_codecs_from_trunk_context() {
    let mut proxy_config = ProxyConfig::default();
    proxy_config.audio_codecs = Some(vec!["pcmu".to_string()]);
    proxy_config.trunks.insert(
        "inbound_192_168_3_7".to_string(),
        TrunkConfig {
            dest: "sip:192.168.3.7:5060".to_string(),
            codec: vec!["pcma".to_string()],
            direction: Some(crate::proxy::routing::TrunkDirection::Inbound),
            inbound_hosts: vec!["192.168.3.7".to_string()],
            ..Default::default()
        },
    );

    let (server, config) = create_test_server_with_config(proxy_config).await;
    let module = CallModule::new(config, server.clone());

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "charlie",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:alice@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:alice@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "charlie".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };
    let cookie = TransactionCookie::default();
    cookie.insert_extension(TrunkContext {
        id: None,
        name: "inbound_192_168_3_7".to_string(),
        did_numbers: vec![],
    });

    let dialplan = module
        .default_resolve(&request, Box::new(NotHandledRouteInvite), &caller, &cookie)
        .await
        .expect("same-realm trunk-originated call should resolve");

    assert_eq!(dialplan.allow_codecs, vec![CodecType::PCMA]);
}

#[tokio::test]
async fn default_resolve_partial_recording_policy_inherits_global_policy_fields() {
    let mut proxy_config = ProxyConfig::default();
    proxy_config.recording = Some(RecordingPolicy {
        samplerate: Some(16000),
        ptime: Some(30),
        stereo_swap: Some(true),
        enabled: Some(true),
        recording_type: Some(crate::config::RecordingType::S3),
        bucket: Some("recordings".to_string()),
        region: Some("us-east-1".to_string()),
        access_key: Some("access".to_string()),
        secret_key: Some("secret".to_string()),
        path: Some("/tmp/rustpbx-main-recordings".to_string()),
        auto_start: Some(true),
        auto_start_at: Some(crate::config::RecordingAutoStartAt::Answer),
        force_file: Some(true),
        ..Default::default()
    });

    let (server, config) = create_test_server_with_config(proxy_config).await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:+12025550100@example.net").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:+12025550100@example.net").unwrap(),
    );

    let caller = SipUser {
        username: "alice".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };
    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RecordingHintsRouteInvite {
                recording: Some(RecordingPolicy {
                    enabled: Some(true),
                    auto_start: Some(false),
                    stereo_swap: Some(false),
                    ..Default::default()
                }),
                enable_recording: None,
            }),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("route should resolve");
    assert_eq!(
        dialplan
            .recording_policy
            .as_ref()
            .and_then(|p| p.auto_start),
        Some(false)
    );
    let dialplan = module.apply_recording_policy(dialplan, &caller);

    assert!(dialplan.recording.enabled);
    assert!(!dialplan.recording.auto_start);
    assert_eq!(
        dialplan.recording.auto_start_at,
        crate::config::RecordingAutoStartAt::Answer
    );
    assert_eq!(
        dialplan.recording.recording_type,
        crate::config::RecordingType::S3
    );
    let option = dialplan
        .recording
        .option
        .expect("merged policy should build recorder option");
    assert_eq!(option.samplerate, Some(16000));
    assert_eq!(option.ptime, Some(30));
    assert_eq!(option.stereo_swap, Some(false));
    assert!(
        option
            .recorder_file
            .starts_with("/tmp/rustpbx-main-recordings"),
        "partial override must inherit the global recorder path"
    );
}

#[tokio::test]
async fn default_resolve_recording_type_sipflow_skips_file_option() {
    let mut proxy_config = ProxyConfig::default();
    proxy_config.recording = Some(RecordingPolicy {
        enabled: Some(true),
        recording_type: Some(crate::config::RecordingType::Sipflow),
        path: Some("/tmp/rustpbx-main-recordings".to_string()),
        ..Default::default()
    });

    let (server, config) = create_test_server_with_config(proxy_config).await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:+12025550100@example.net").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:+12025550100@example.net").unwrap(),
    );

    let caller = SipUser {
        username: "alice".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };
    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RecordingHintsRouteInvite {
                recording: None,
                enable_recording: None,
            }),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("route should resolve");
    let dialplan = module.apply_recording_policy(dialplan, &caller);

    assert!(dialplan.recording.enabled);
    assert_eq!(
        dialplan.recording.recording_type,
        crate::config::RecordingType::Sipflow
    );
    assert!(
        dialplan.recording.option.is_none(),
        "type=sipflow must not install a WAV file option"
    );
}

#[tokio::test]
async fn default_resolve_recording_enable_hint_false_disables_global_policy() {
    let mut proxy_config = ProxyConfig::default();
    proxy_config.recording = Some(RecordingPolicy {
        enabled: Some(true),
        path: Some("/tmp/rustpbx-main-recordings".to_string()),
        auto_start: Some(true),
        ..Default::default()
    });

    let (server, config) = create_test_server_with_config(proxy_config).await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:+12025550100@example.net").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:+12025550100@example.net").unwrap(),
    );

    let caller = SipUser {
        username: "alice".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };
    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RecordingHintsRouteInvite {
                recording: None,
                enable_recording: Some(false),
            }),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("route should resolve");
    assert_eq!(
        dialplan.recording_policy.as_ref().and_then(|p| p.enabled),
        Some(false)
    );
    let dialplan = module.apply_recording_policy(dialplan, &caller);

    assert!(!dialplan.recording.enabled);
    assert!(dialplan.recording.option.is_none());
}

#[tokio::test]
async fn default_route_uses_acl_context_for_source_trunk() {
    let mut proxy_config = ProxyConfig::default();
    proxy_config.generated_dir =
        format!("target/test-generated/source-route-{}", std::process::id());
    proxy_config.trunks.insert(
        "via_source".to_string(),
        TrunkConfig {
            dest: "sip:5.5.5.5:5060".to_string(),
            direction: Some(crate::proxy::routing::TrunkDirection::Inbound),
            inbound_hosts: vec!["5.5.5.0/24".to_string()],
            ..Default::default()
        },
    );
    proxy_config.trunks.insert(
        "real_source".to_string(),
        TrunkConfig {
            dest: "sip:1.2.3.4:5060".to_string(),
            direction: Some(crate::proxy::routing::TrunkDirection::Inbound),
            inbound_hosts: vec!["1.2.3.0/24".to_string()],
            ..Default::default()
        },
    );
    proxy_config.trunks.insert(
        "wrong_carrier".to_string(),
        TrunkConfig {
            dest: "sip:10.0.0.1:5060".to_string(),
            direction: Some(crate::proxy::routing::TrunkDirection::Outbound),
            ..Default::default()
        },
    );
    proxy_config.trunks.insert(
        "right_carrier".to_string(),
        TrunkConfig {
            dest: "sip:10.0.0.2:5060".to_string(),
            direction: Some(crate::proxy::routing::TrunkDirection::Outbound),
            ..Default::default()
        },
    );
    proxy_config.routes = Some(vec![
        RouteRule {
            name: "wrong-via-source".to_string(),
            priority: 0,
            source_trunks: vec!["via_source".to_string()],
            match_conditions: crate::proxy::routing::MatchConditions::default(),
            action: crate::proxy::routing::RouteAction {
                dest: Some(crate::proxy::routing::DestConfig::Single(
                    "wrong_carrier".to_string(),
                )),
                ..Default::default()
            },
            ..Default::default()
        },
        RouteRule {
            name: "right-real-source".to_string(),
            priority: 10,
            source_trunks: vec!["real_source".to_string()],
            match_conditions: crate::proxy::routing::MatchConditions::default(),
            action: crate::proxy::routing::RouteAction {
                dest: Some(crate::proxy::routing::DestConfig::Single(
                    "right_carrier".to_string(),
                )),
                ..Default::default()
            },
            ..Default::default()
        },
    ]);

    let (server, _config) = create_test_server_with_config(proxy_config).await;
    let route_invite = DefaultRouteInvite {
        routing_state: std::sync::Arc::new(RoutingState::default()),
        data_context: server.data_context.clone(),
    };

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:callee@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:callee@rustpbx.com").unwrap(),
    );
    request
        .headers
        .retain(|header| !matches!(header, rsipstack::sip::Header::Via(_)));
    request.headers.push(rsipstack::sip::Header::Via(
        rsipstack::sip::headers::Via::new("SIP/2.0/UDP 5.5.5.5:5060;branch=z9hG4bK-wrong-via"),
    ));

    let option = InviteOption {
        caller: rsipstack::sip::Uri::try_from("sip:caller@1.2.3.4").unwrap(),
        callee: request.uri.clone(),
        ..Default::default()
    };
    let cookie = TransactionCookie::default();
    cookie.insert_extension(TrunkContext {
        id: None,
        name: "real_source".to_string(),
        did_numbers: vec![],
    });

    let result = route_invite
        .route_invite(option, &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("route invite should succeed");

    match result {
        RouteResult::Forward(mut targets) => {
            assert_eq!(targets.len(), 1);
            let option = targets.pop().unwrap().option;
            assert_eq!(
                option.destination.unwrap().addr.to_string(),
                "10.0.0.2:5060"
            );
        }
        RouteResult::NotHandled(_, _) => {
            panic!("expected Forward via right carrier, got NotHandled")
        }
        RouteResult::Abort(code, reason) => panic!(
            "expected Forward via right carrier, got Abort {} {:?}",
            code, reason
        ),
        RouteResult::Queue { .. } => panic!("expected Forward via right carrier, got Queue"),
        RouteResult::Application { .. } => {
            panic!("expected Forward via right carrier, got Application")
        }
    }
}

#[test]
fn test_parse_replaces_header_basic() {
    let mut request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: "sip:replace@example.com".try_into().unwrap(),
        version: rsipstack::sip::Version::V2,
        headers: vec![].into(),
        body: vec![],
    };
    request.headers.push(rsipstack::sip::Header::Other(
        "Replaces".into(),
        "abc123;to-tag=local456;from-tag=remote789".into(),
    ));

    let result = CallModule::parse_replaces_header(&request);
    assert!(result.is_some());
    let (call_id, to_tag, from_tag) = result.unwrap();
    assert_eq!(call_id, "abc123");
    assert_eq!(to_tag, "local456");
    assert_eq!(from_tag, "remote789");
}

#[test]
fn test_parse_replaces_header_missing() {
    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: "sip:replace@example.com".try_into().unwrap(),
        version: rsipstack::sip::Version::V2,
        headers: vec![].into(),
        body: vec![],
    };

    let result = CallModule::parse_replaces_header(&request);
    assert!(result.is_none());
}

#[tokio::test]
async fn default_resolve_uses_request_uri_for_same_realm_detection() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("offline internal user should not error — 480 deferred to build_dialplan");

    assert!(
        dialplan.is_empty(),
        "offline same-realm should have empty targets"
    );
    assert!(
        dialplan.extensions.get::<CalleeOfflineMarker>().is_some(),
        "offline same-realm should have offline marker set"
    );
}

#[tokio::test]
async fn default_resolve_falls_through_external_callee_to_locs() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri =
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("external destination should fall through to locs");

    use crate::call::DialplanFlow;
    match &dialplan.flow {
        DialplanFlow::Targets(strategy) => {
            assert!(!strategy.is_empty(), "external callee should be in targets");
        }
        _ => panic!("expected Targets flow for external callee"),
    }
}

#[tokio::test]
async fn build_dialplan_sends_offline_callee_to_voicemail_unless_opted_out() {
    // (user exists, voicemail_disabled)
    for (exists, disabled) in [(true, false), (true, true), (false, false)] {
        let (server, config) = create_test_server().await;
        if exists {
            server
                .user_backend
                .create_user(SipUser {
                    id: 99,
                    username: "vm-offline".to_string(),
                    enabled: true,
                    realm: Some("rustpbx.com".to_string()),
                    voicemail_disabled: disabled,
                    ..Default::default()
                })
                .await
                .expect("create callee");
        }
        let module = CallModule::new(config, server);
        let mut request = crate::proxy::tests::common::create_test_request(
            rsipstack::sip::Method::Invite,
            "bp",
            None,
            "rustpbx.com",
            None,
        );
        let callee_uri = rsipstack::sip::Uri::try_from("sip:vm-offline@rustpbx.com").unwrap();
        request.uri = callee_uri.clone();
        replace_to_header(&mut request, callee_uri);
        let (mut tx, _endpoint) =
            crate::proxy::tests::common::create_transaction(request).await;
        let caller = SipUser {
            username: "bp".to_string(),
            realm: Some("rustpbx.com".to_string()),
            ..Default::default()
        };

        let result = module
            .build_dialplan(&mut tx, TransactionCookie::default(), &caller)
            .await;
        if exists && !disabled {
            let plan = result.expect("offline callee should reach voicemail");
            let DialplanFlow::Application { app_name, app_params, .. } = plan.flow else {
                panic!("offline callee must go straight to the voicemail app");
            };
            assert_eq!(app_name, "voicemail");
            assert_eq!(
                app_params,
                Some(serde_json::json!({ "extension": "vm-offline" }))
            );
        } else {
            let err = result.expect_err("unknown or opted-out callee must not reach voicemail");
            assert_eq!(
                err.status,
                Some(rsipstack::sip::StatusCode::TemporarilyUnavailable)
            );
        }
    }
}

#[tokio::test]
async fn default_resolve_always_forwarding_uri_bypasses_offline_locator() {
    let (server, config) = create_test_server().await;
    server
        .user_backend
        .create_user(SipUser {
            id: 99,
            username: "cfwd".to_string(),
            enabled: true,
            realm: Some("rustpbx.com".to_string()),
            call_forwarding_mode: Some("always".to_string()),
            call_forwarding_destination: Some("sip:alice@rustpbx.com".to_string()),
            ..Default::default()
        })
        .await
        .expect("create forwarding user");
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:cfwd@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:cfwd@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("always forwarding should bypass offline locator check");

    let target = dialplan
        .first_target()
        .expect("forwarding target should be present")
        .aor
        .to_string();
    assert_eq!(target, "sip:alice@rustpbx.com");
}

#[tokio::test]
async fn default_resolve_always_forwarding_route_point_uses_application_route_context() {
    let (server, mut config) = create_test_server().await;
    Arc::make_mut(&mut config).frequency_limiter = Some("memory".to_string());
    server
        .user_backend
        .create_user(SipUser {
            id: 101,
            username: "cfwdrp".to_string(),
            enabled: true,
            realm: Some("rustpbx.com".to_string()),
            call_forwarding_mode: Some("always".to_string()),
            call_forwarding_destination: Some("toivr:39230".to_string()),
            ..Default::default()
        })
        .await
        .expect("create route-point forwarding user");
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    let route_point_user_uri = format!("sip:{}@{}", "cfwdrp", "rustpbx.com");
    request.uri = rsipstack::sip::Uri::try_from(route_point_user_uri.as_str()).unwrap();
    let request_uri = request.uri.clone();
    replace_to_header(&mut request, request_uri);

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };
    let dialplan = module
        .default_resolve(
            &request,
            Box::new(RoutePointApplicationRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("route-point forwarding should resolve through call routing");

    match &dialplan.flow {
        crate::call::DialplanFlow::Application {
            app_name,
            app_params,
            auto_answer,
        } => {
            assert_eq!(app_name, "step_ivr");
            assert_eq!(
                app_params
                    .as_ref()
                    .and_then(|params| params.get("businessKey")),
                Some(&serde_json::json!("driver-spring-festival"))
            );
            assert!(*auto_answer);
        }
        other => panic!("expected application flow, got {other:?}"),
    }
    assert_eq!(
        dialplan
            .routed_headers
            .as_ref()
            .and_then(|headers| headers.iter().find_map(|header| match header {
                rsipstack::sip::Header::Other(name, value)
                    if name.eq_ignore_ascii_case("X-Business-Key") =>
                {
                    Some(value.as_str())
                }
                _ => None,
            })),
        Some("driver-spring-festival")
    );

    let limiter = module
        .inner
        .routing_state
        .policy_guard
        .as_ref()
        .expect("frequency policy guard")
        .limiter()
        .clone();
    assert!(
        limiter
            .check_concurrency("route-point-policy", "caller", "bp", 1)
            .await
            .expect("acquire route-point concurrency hold")
    );
    let error = module
        .default_resolve(
            &request,
            Box::new(RoutePointNotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect_err("unresolved route point must not dial the forwarding user");
    assert_eq!(
        error.status,
        Some(rsipstack::sip::StatusCode::ServerInternalError)
    );
    assert!(
        limiter
            .check_concurrency("route-point-policy", "caller", "bp", 1)
            .await
            .expect("reacquire released route-point concurrency hold")
    );
    limiter
        .release_concurrency("route-point-policy", "caller", "bp")
        .await
        .expect("release test concurrency hold");
}

#[tokio::test]
async fn default_resolve_always_forwarding_queue_missing_returns_480() {
    let (server, config) = create_test_server().await;
    server
        .user_backend
        .create_user(SipUser {
            id: 100,
            username: "cfwdq".to_string(),
            enabled: true,
            realm: Some("rustpbx.com".to_string()),
            call_forwarding_mode: Some("always".to_string()),
            call_forwarding_destination: Some("queue:99999".to_string()),
            ..Default::default()
        })
        .await
        .expect("create queue forwarding user");
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:cfwdq@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:cfwdq@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let err = module
        .default_resolve(
            &request,
            Box::new(NotHandledRouteInvite),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect_err("missing always-forwarding queue should fail");

    assert_eq!(
        err.status,
        Some(rsipstack::sip::StatusCode::TemporarilyUnavailable)
    );
    assert!(err.error.to_string().contains("queue '99999' not found"));
}

#[test]
fn resolve_callee_uri_prefers_request_uri() {
    let request_uri = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();
    let to_uri =
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: request_uri.clone(),
        version: rsipstack::sip::Version::V2,
        headers: vec![
            rsipstack::sip::typed::To {
                display_name: None,
                uri: to_uri,
                params: vec![],
            }
            .into(),
        ]
        .into(),
        body: vec![],
    };

    let resolved = resolve_callee_uri(&request).expect("expected callee uri");
    assert_eq!(resolved, request_uri);
}

#[test]
fn resolve_callee_uri_falls_back_to_to_header() {
    let request_uri = rsipstack::sip::Uri::try_from("sip:rustpbx.com").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: request_uri,
        version: rsipstack::sip::Version::V2,
        headers: vec![
            rsipstack::sip::typed::To {
                display_name: None,
                uri: to_uri.clone(),
                params: vec![],
            }
            .into(),
        ]
        .into(),
        body: vec![],
    };

    let resolved = resolve_callee_uri(&request).expect("expected callee uri");
    assert_eq!(resolved, to_uri);
}

#[tokio::test]
async fn default_resolve_application_carries_routed_headers() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let mut request = crate::proxy::tests::common::create_test_request(
        rsipstack::sip::Method::Invite,
        "bp",
        None,
        "rustpbx.com",
        None,
    );
    request.uri = rsipstack::sip::Uri::try_from("sip:ivrapp@rustpbx.com").unwrap();
    replace_to_header(
        &mut request,
        rsipstack::sip::Uri::try_from("sip:ivrapp@rustpbx.com").unwrap(),
    );

    let caller = SipUser {
        username: "bp".to_string(),
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let expected_headers = Some(vec![
        rsipstack::sip::Header::Other("X-Custom".to_string(), "custom-value".to_string()),
        rsipstack::sip::Header::Other(
            "P-Asserted-Identity".to_string(),
            "<sip:routing@pbx.com>".to_string(),
        ),
    ]);

    let dialplan = module
        .default_resolve(
            &request,
            Box::new(ApplicationRouteInvite {
                headers: expected_headers.clone(),
            }),
            &caller,
            &TransactionCookie::default(),
        )
        .await
        .expect("application route should resolve");

    assert!(
        matches!(dialplan.flow, crate::call::DialplanFlow::Application { .. }),
        "expected Application flow"
    );
    assert_eq!(
        dialplan.routed_headers, expected_headers,
        "routed headers should be preserved in Dialplan"
    );
}

#[tokio::test]
async fn test_build_dialplan_resolves_same_realm_target_destination() {
    use rsipstack::sip::HostWithPort;
    use rsipstack::transport::SipAddr;

    let (server, _config) = create_test_server().await;

    let aor: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().expect("valid URI");

    let expected_destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: HostWithPort::try_from("192.168.1.100:5060").expect("valid address"),
    };

    // Register alice in the memory locator
    server
        .locator
        .register(
            "alice",
            Some("rustpbx.com"),
            Location {
                aor: aor.clone(),
                expires: 3600,
                destination: Some(expected_destination.clone()),
                supports_webrtc: true,
                last_modified: Some(std::time::Instant::now()),
                ..Default::default()
            },
        )
        .await
        .expect("register should succeed");

    // Build a minimal dialplan with a same-realm target (destination: None)
    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: "sip:target@rustpbx.com".try_into().unwrap(),
        headers: vec![
            rsipstack::sip::Header::From("sip:caller@rustpbx.com".into()),
            rsipstack::sip::Header::To("sip:target@rustpbx.com".into()),
            rsipstack::sip::Header::CallId("test-call".into()),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: vec![],
    };
    let mut dialplan =
        Dialplan::new("test-session".to_string(), request, DialDirection::Internal)
            .with_targets(DialStrategy::Parallel(vec![Location {
                aor: aor.clone(),
                destination: None,
                ..Default::default()
            }]));

    // Run the same resolution logic from build_dialplan
    if let DialplanFlow::Targets(ref mut strategy) = dialplan.flow {
        let targets = match strategy {
            DialStrategy::Parallel(t) => t,
            DialStrategy::Sequential(_) => panic!("expected parallel targets"),
        };
        for target in targets.iter_mut() {
            if target.destination.is_none() {
                let realm = target.aor.host().to_string();
                if server.is_same_realm(&realm).await {
                    if let Ok(locs) = server.locator.lookup(&target.aor).await {
                        if let Some(loc) = locs.first() {
                            if let Some(dest) = loc.destination.clone() {
                                target.destination = Some(dest);
                                target.supports_webrtc = loc.supports_webrtc;
                                if target.transport.is_none() {
                                    target.transport = loc.transport;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Verify destination was resolved via locator
    let targets = match &dialplan.flow {
        DialplanFlow::Targets(strategy) => match strategy {
            DialStrategy::Parallel(t) => t,
            DialStrategy::Sequential(_) => panic!("expected parallel targets"),
        },
        _ => panic!("Expected Targets flow"),
    };
    assert_eq!(targets.len(), 1);
    let resolved = targets[0]
        .destination
        .as_ref()
        .expect("destination should be resolved via locator");
    assert_eq!(resolved.addr.to_string(), "192.168.1.100:5060");
    assert_eq!(resolved.r#type, Some(rsipstack::sip::Transport::Udp));
    assert!(
        targets[0].supports_webrtc,
        "supports_webrtc must propagate from locator result into target"
    );
}



#[tokio::test]
async fn test_build_dialplan_skips_external_realm_targets() {
    use rsipstack::sip::HostWithPort;
    use rsipstack::transport::SipAddr;

    let (server, _config) = create_test_server().await;

    // Register alice (same-realm) in locator
    let alice_aor: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().expect("valid URI");
    let alice_dest = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: HostWithPort::try_from("192.168.1.100:5060").expect("valid address"),
    };
    server
        .locator
        .register(
            "alice",
            Some("rustpbx.com"),
            Location {
                aor: alice_aor.clone(),
                expires: 3600,
                destination: Some(alice_dest.clone()),
                last_modified: Some(std::time::Instant::now()),
                ..Default::default()
            },
        )
        .await
        .expect("register should succeed");

    // Build dialplan with two targets: one same-realm, one external
    let external_aor: rsipstack::sip::Uri =
        "sip:bob@external.com".try_into().expect("valid URI");

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: "sip:target@rustpbx.com".try_into().unwrap(),
        headers: vec![
            rsipstack::sip::Header::From("sip:caller@rustpbx.com".into()),
            rsipstack::sip::Header::To("sip:target@rustpbx.com".into()),
            rsipstack::sip::Header::CallId("test-call-ext".into()),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: vec![],
    };
    let mut dialplan = Dialplan::new(
        "test-session-ext".to_string(),
        request,
        DialDirection::Internal,
    )
    .with_targets(DialStrategy::Parallel(vec![
        Location {
            aor: alice_aor.clone(),
            destination: None,
            ..Default::default()
        },
        Location {
            aor: external_aor.clone(),
            destination: None,
            ..Default::default()
        },
    ]));

    // Run the same resolution logic
    if let DialplanFlow::Targets(ref mut strategy) = dialplan.flow {
        let targets = match strategy {
            DialStrategy::Parallel(t) => t,
            DialStrategy::Sequential(_) => panic!("expected parallel targets"),
        };
        for target in targets.iter_mut() {
            if target.destination.is_none() {
                let realm = target.aor.host().to_string();
                if server.is_same_realm(&realm).await {
                    if let Ok(locs) = server.locator.lookup(&target.aor).await {
                        if let Some(loc) = locs.first() {
                            if let Some(dest) = loc.destination.clone() {
                                target.destination = Some(dest);
                                target.supports_webrtc = loc.supports_webrtc;
                                if target.transport.is_none() {
                                    target.transport = loc.transport;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Verify same-realm target got resolved, external target stays None
    let targets = match &dialplan.flow {
        DialplanFlow::Targets(strategy) => match strategy {
            DialStrategy::Parallel(t) => t,
            DialStrategy::Sequential(_) => panic!("expected parallel targets"),
        },
        _ => panic!("Expected Targets flow"),
    };
    assert_eq!(targets.len(), 2);

    // alice (same-realm) should have destination resolved
    assert!(
        targets[0].destination.is_some(),
        "same-realm target should have destination resolved"
    );

    // bob (external) should NOT have destination set
    assert!(
        targets[1].destination.is_none(),
        "external-realm target should NOT have destination resolved"
    );
}

// ── cluster child-leg session id contract ───────────────────────────────

/// A child-leg INVITE exactly as the DISPATCHING node's production builders
/// serialize it (`initiate_sip_leg_with_deadline`): the agent-leg Call-ID
/// from `agent_leg_call_id`, the root id stamped on the `Session-ID`
/// header (`build_wire_header`) and the CC UUI `leg=agent` dial intent
/// (`build_uui_header`). No hand-written correlation formats here — the
/// parsing side must accept what the sending side actually emits.
fn child_leg_invite(call_id: &str, root: &str) -> rsipstack::sip::Request {
    let mut headers = rsipstack::sip::Headers::default();
    headers.push(rsipstack::sip::Header::CallId(
        rsipstack::sip::headers::CallId::new(call_id.to_string()),
    ));
    // The dispatching node stamps the root id on the `Session-ID` header —
    // a 32-hex root rides `InviteOption.session_id`, which rsipstack
    // serializes on the wire through `SessionId::from_local`.
    headers.push(
        rsipstack::sip::headers::SessionId::from_local(root)
            .expect("32-hex root passes rsipstack validation")
            .into(),
    );
    headers.push(crate::call::uui::build_uui_header(
        root,
        None,
        None,
        None,
        Some("agent"),
    ));
    rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: "sip:1001@rustpbx.com".parse().unwrap(),
        version: rsipstack::sip::Version::V2,
        headers,
        body: Vec::new(),
    }
}

async fn peer_server_and_cookie() -> (SipServerRef, TransactionCookie) {
    let (mut server, _config) = create_test_server().await;
    Arc::get_mut(&mut server).unwrap().cluster_peer_ips =
        vec!["192.0.2.44".parse().unwrap()];
    let cookie = TransactionCookie::default();
    cookie.insert_extension(crate::call::CallerPeerContext("192.0.2.44:5060".into()));
    (server, cookie)
}

/// REPRO (queue re-dial loses the call id): when the first offered agent
/// misses, the re-dial INVITE cannot reuse the Call-ID the phone already
/// declined — it dials `{root}-r2`. The agent-home node terminates that
/// INVITE as its own session and, per the CTI contract, the agent's
/// answered session must keep the ROOT id: every event / CDR of the
/// logical call shares one `call_id`. A fresh per-leg id here is exactly
/// the "sessionid changed when the agent picked up" divergence.
#[tokio::test]
async fn redial_child_leg_session_id_adopts_root() {
    let root = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let (server, cookie) = peer_server_and_cookie().await;
    // Second agent-facing dial of the session → wire Call-ID `{root}-r2`.
    let call_id = SipSession::agent_leg_call_id(root, 1);
    assert_eq!(call_id, format!("{root}-r2"));
    let request = child_leg_invite(&call_id, root);
    let resolved = CallModule::resolve_inbound_session_id(
        &server, &cookie, &request, &call_id, true,
    );
    assert_eq!(
        resolved, root,
        "a cluster child leg must adopt the root session id, not a fresh per-leg id"
    );
}

/// The session-id layer can be disabled for the child INVITE (plain
/// internal dialplan, no app/queue): the legacy fallback is the RAW
/// Call-ID — `{root}-r2` verbatim. Adoption must work there too, since
/// that is the branch queue dispatch actually takes on the agent node.
#[tokio::test]
async fn redial_child_leg_adopts_root_even_without_session_id_layer() {
    let root = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let (server, cookie) = peer_server_and_cookie().await;
    let call_id = SipSession::agent_leg_call_id(root, 1);
    let request = child_leg_invite(&call_id, root);
    let resolved = CallModule::resolve_inbound_session_id(
        &server, &cookie, &request, &call_id, false,
    );
    assert_eq!(
        resolved, root,
        "raw-Call-ID fallback must not leak the -r2 suffix into the session id"
    );
}

/// First dial (seq == 0): the wire Call-ID IS the root id, so the legacy
/// Call-ID derivation already lands on it. Pins the case that was never
/// broken — the closed loop from `agent_leg_call_id` to the inbound
/// derivation.
#[tokio::test]
async fn first_dial_child_leg_keeps_root_via_call_id() {
    let root = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let (server, cookie) = peer_server_and_cookie().await;
    let call_id = SipSession::agent_leg_call_id(root, 0);
    assert_eq!(call_id, root);
    let request = child_leg_invite(&call_id, root);
    let resolved = CallModule::resolve_inbound_session_id(
        &server, &cookie, &request, &call_id, true,
    );
    assert_eq!(resolved, root);
}

/// A non-peer source (external PBX planting the same headers) must NOT
/// get its id adopted — the legacy derivation keeps a unique id.
#[tokio::test]
async fn non_peer_redial_keeps_unique_id() {
    let root = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let (mut server, _config) = create_test_server().await;
    Arc::get_mut(&mut server).unwrap().cluster_peer_ips =
        vec!["192.0.2.44".parse().unwrap()];
    // No CallerPeerContext: the INVITE did not arrive from a peer.
    let cookie = TransactionCookie::default();
    let call_id = SipSession::agent_leg_call_id(root, 1);
    let request = child_leg_invite(&call_id, root);
    let resolved = CallModule::resolve_inbound_session_id(
        &server, &cookie, &request, &call_id, true,
    );
    assert_ne!(resolved, root);
    assert_eq!(
        resolved.len(),
        32,
        "legacy derivation must still yield an id-shaped session id"
    );
}

/// Parallel agent legs on ONE node: a live local session already holds
/// the root id, so adoption must back off — parallel legs must not
/// collapse into one session id. (They still correlate through the
/// enriched `session_id` field; uniqueness wins here.)
#[tokio::test]
async fn live_local_root_id_blocks_adoption() {
    use crate::proxy::active_call_registry::ActiveProxyCallEntry;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;

    let root = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let (server, cookie) = peer_server_and_cookie().await;
    // A live local session occupying the root id (parallel leg already
    // serving on this node).
    let dialplan = Dialplan::new(
        "parallel-leg-probe".to_string(),
        crate::proxy::tests::common::create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        ),
        DialDirection::Inbound,
    );
    let (_session, handle, _rx) = build_session_with_cmd_rx_on(server.clone(), dialplan).await;
    server.active_call_registry.upsert(
        ActiveProxyCallEntry {
            session_id: root.to_string(),
            caller: None,
            callee: None,
            direction: "inbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        },
        handle,
    );

    let call_id = SipSession::agent_leg_call_id(root, 1);
    let request = child_leg_invite(&call_id, root);
    let resolved = CallModule::resolve_inbound_session_id(
        &server, &cookie, &request, &call_id, true,
    );
    assert_ne!(
        resolved, root,
        "a live local session with the root id must block adoption"
    );
}
