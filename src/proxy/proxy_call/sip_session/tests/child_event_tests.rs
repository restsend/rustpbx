//! Child-dialog events must reflect the current endpoint and published hook context.
use super::*;
use crate::call::{DialDirection, Dialplan, Location};
use crate::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};
use crate::proxy::tests::test_sip_session_regressions::{
    build_session_with_cmd_rx_on, setup_recording_test_media,
};

/// Model an addon resolving identity at ringing, without coupling core tests to CC.
struct RingingIdentity;

#[tokio::test]
async fn scoped_transfer_review_keeps_selected_agent_identity() {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry};
    let registry = Arc::new(MemoryRegistry::new());
    // A is a canonical agent ID; the same text is also B's dialable endpoint.
    registry
        .register(
            "agent-a".into(),
            "A".into(),
            "sip:agent-a@other.test".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    registry
        .register(
            "agent-b".into(),
            "B".into(),
            "sip:agent-a@rustpbx.com".into(),
            vec![],
            1,
        )
        .await
        .unwrap();
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (mut server, _) = create_test_server_with_rwi_gateway(
        crate::config::ProxyConfig {
            route_originated_calls: false,
            ..Default::default()
        },
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    Arc::get_mut(&mut server).unwrap().agent_registry = Some(registry);
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new("selected-agent".into(), request, DialDirection::Inbound)
        .with_caller("sip:alice@rustpbx.com".parse().unwrap());
    let (mut session, _, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;
    session.extensions.set_agent_attribution("agent-a");
    let leg_id = LegId::from("selected-agent");
    let added = session
        .execute_command(
            CallCommand::LegAdd {
                source_leg: Some(LegId::from("caller")),
                target: "sip:agent-a@rustpbx.com".into(),
                leg_id: Some(leg_id.clone()),
                headers: vec![],
                purpose: Some(crate::call::domain::LegPurpose::Agent),
            },
            None,
        )
        .await;
    assert!(added.success, "{added:?}");
    session.update_leg_state(&leg_id, LegState::Ringing);
    let mut ringing = Vec::new();
    while let Ok(entry) = events.try_recv() {
        if entry.event.event_type == "call_ringing" {
            ringing.push(entry.event);
        }
    }
    assert_eq!(ringing.len(), 1);
    assert_eq!(ringing[0].payload["agent_id"], "agent-a");
    session.cleanup().await;
}

#[tokio::test]
async fn scoped_transfer_review_rejected_route_keeps_concurrency_available() {
    use crate::call::policy::{FrequencyLimiter, InMemoryFrequencyLimiter, PolicyGuard};
    use crate::models::policy::{ConcurrencyLimit, PolicySpec};
    let limiter = InMemoryFrequencyLimiter::new();
    let mut config = super::test_forward_route_config();
    config.route_originated_calls = false;
    config.routes.as_mut().unwrap()[0].policy = Some(PolicySpec {
        concurrency: Some(ConcurrencyLimit {
            max_total: 1,
            max_per_account: HashMap::new(),
        }),
        ..Default::default()
    });
    let (mut server, _) = create_test_server_with_rwi_gateway(
        config,
        Arc::new(parking_lot::RwLock::new(
            crate::rwi::gateway::RwiGateway::new(),
        )),
    )
    .await;
    Arc::get_mut(&mut server).unwrap().frequency_limiter = Some(limiter.clone());
    let mut routing = crate::call::RoutingState::new();
    routing.policy_guard = Some(Arc::new(PolicyGuard::new(limiter.clone())));
    *server.routing_state.write() = Arc::new(routing);
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new("rejected-transfer".into(), request, DialDirection::Inbound)
        .with_caller("sip:alice@rustpbx.com".parse().unwrap());
    let (mut session, _, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;
    let mut agent = crate::call::domain::Leg::new(LegId::from("callee"));
    agent.agent_id = Some("agent-a".into());
    agent.state = LegState::Connected;
    session.legs.insert(LegId::from("callee"), agent);
    let (_, mut states) = mpsc::unbounded_channel();
    let result = session
        .execute_command(
            CallCommand::BlindTransferCustomer {
                target: "sip:9000@rustpbx.com".into(),
                headers: HashMap::new(),
                application_only: true,
                operator_agent: None,
                operator_leg_hint: None,
                completion: None,
            },
            Some(&mut states),
        )
        .await;
    assert!(!result.success);
    assert!(session.legs.contains_key(&LegId::from("callee")));
    assert!(
        limiter
            .check_concurrency("outbound-gw", "caller", "alice", 1)
            .await
            .unwrap(),
        "a rejected transfer must leave the routing concurrency slot available"
    );
    limiter
        .release_concurrency("outbound-gw", "caller", "alice")
        .await
        .unwrap();
    session.cleanup().await;
}

#[async_trait::async_trait]
impl CallSessionHook for RingingIdentity {
    async fn on_call_ringing(&self, ctx: &CallSessionContext, _: bool) {
        ctx.extensions.set_agent_attribution("agent-1");
        ctx.extensions.write().insert(HashMap::from([
            ("agent_id".to_string(), "agent-1".to_string()),
            ("agent_name".to_string(), "Agent One".to_string()),
        ]));
    }
}

#[tokio::test]
async fn child_first_provisional_event_contains_published_identity() {
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::udp::UdpConnection;

    for early_media in [false, true] {
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let mut gateway = crate::rwi::gateway::RwiGateway::new();
        gateway.set_client_ip_lookup(Some(Arc::new(|id| {
            (id == "agent-1").then(|| "192.0.2.10".into())
        })));
        gateway.set_recording_identity_lookup(Some(Arc::new(|_, id, _| {
            (id == "agent-1").then(|| "recording-agent-1".into())
        })));
        let mut events = gateway.subscribe_events();
        let mut provisional_events = gateway.subscribe_events();
        let (mut server, _) = create_test_server_with_rwi_gateway(
            Default::default(),
            Arc::new(parking_lot::RwLock::new(gateway)),
        )
        .await;
        Arc::get_mut(&mut server).unwrap().session_hooks =
            Arc::new(vec![Arc::new(RingingIdentity)]);
        let endpoint = server.endpoint.inner.clone();
        for local in endpoint.transport_layer.get_addrs() {
            endpoint.transport_layer.del_transport(&local);
        }
        let transport =
            UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
                .await
                .unwrap();
        endpoint.transport_layer.add_transport(transport.into());
        let serving_endpoint = endpoint.clone();
        let serving = tokio::spawn(async move { serving_endpoint.serve().await.unwrap() });
        let request = create_test_request(Method::Invite, "customer", None, "example.test", None);
        let dialplan = Dialplan::new("child-first-ring".into(), request, DialDirection::Inbound)
            .with_caller("sip:customer@example.test".parse().unwrap());
        let (mut session, _handle, _commands) =
            build_session_with_cmd_rx_on(server, dialplan).await;
        setup_recording_test_media(&mut session).await;
        // The trusted peer declares a child role; endpoint identity is resolved by the hook.
        session.meta.child_leg_role = Some("agent".into());
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(state_tx);
        let responder = tokio::spawn(async move {
            let mut buffer = [0u8; 65536];
            let (size, source) =
                tokio::time::timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            let SipMessage::Request(invite) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
            else {
                panic!("expected INVITE");
            };
            for status in [
                if early_media {
                    StatusCode::SessionProgress
                } else {
                    StatusCode::Ringing
                },
                StatusCode::BusyHere,
            ] {
                let mut response = endpoint.make_response(&invite, status.clone(), None);
                response
                    .headers
                    .retain(|header| !matches!(header, Header::To(_)));
                response.headers.push(Header::To(
                    format!("<sip:target@{address}>;tag=ring-target").into(),
                ));
                if early_media && status == StatusCode::SessionProgress {
                    response
                        .headers
                        .push(Header::ContentType("application/sdp".into()));
                    response.body = b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_vec();
                }
                response
                    .headers
                    .retain(|header| !matches!(header, Header::ContentLength(_)));
                response.headers.push(Header::ContentLength(
                    response.body.len().to_string().into(),
                ));
                target
                    .send_to(response.to_string().as_bytes(), source)
                    .await
                    .unwrap();
                if status != StatusCode::BusyHere {
                    // Observe delivery instead of racing a final response against a sleep.
                    tokio::time::timeout(Duration::from_secs(3), async {
                        loop {
                            let entry = provisional_events.recv().await.unwrap();
                            if entry.event.event_type == "call_ringing"
                                && entry.event.payload["leg_id"] == "callee"
                            {
                                break;
                            }
                        }
                    })
                    .await
                    .expect("provisional must deliver a leg event");
                }
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            session.try_single_target(
                &Location {
                    aor: format!("sip:target@{address}").parse().unwrap(),
                    ..Default::default()
                },
                &mut state_rx,
                None,
                None,
                None,
            ),
        )
        .await
        .expect("callee must receive the provisional and rejection");
        responder.await.unwrap();
        serving.abort();
        assert!(result.is_err());
        let mut ringing = Vec::new();
        while let Ok(entry) = events.try_recv() {
            if entry.event.event_type == "call_ringing" {
                ringing.push(entry.event.payload);
            }
        }
        session.cleanup().await;
        assert_eq!(
            ringing.len(),
            1,
            "child ringing must remain unique: {ringing:?}"
        );
        let event = &ringing[0];
        assert_eq!(event["call_id"], "test-session");
        assert_eq!(event["leg_id"], "callee");
        assert_eq!(event["leg_role"], "agent");
        assert_eq!(event["early_media"], early_media);
        assert_eq!(
            event["agent_id"], "agent-1",
            "first delivered event: {event}"
        );
        assert_eq!(event["agent_name"], "Agent One");
        assert_eq!(event["recording_id"], "recording-agent-1");
        assert_eq!(event["client_ip"], "192.0.2.10");
    }
}

#[tokio::test]
async fn child_added_legs_keep_their_own_role_and_registration_owner() {
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "customer",
        None,
        "example.test",
        None,
    );
    let dialplan = Dialplan::new("child-target-role".into(), request, DialDirection::Inbound);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;
    session.meta.child_leg_role = Some("agent".into());
    session.meta.transfer_in_progress = true;
    for (name, purpose, remote, expected) in [
        ("transfer-local", None, false, Some("callee")),
        (
            "consult-local",
            Some(crate::call::domain::LegPurpose::Consult),
            false,
            Some("consult"),
        ),
        ("transfer-remote", None, true, None),
    ] {
        let leg_id = LegId::from(name);
        let added = session
            .execute_command(
                CallCommand::LegAdd {
                    source_leg: Some(LegId::from("caller")),
                    target: "sip:target@127.0.0.1:5099".into(),
                    leg_id: Some(leg_id.clone()),
                    headers: vec![],
                    purpose,
                },
                None,
            )
            .await;
        assert!(added.success, "{added:?}");
        session.legs.get_mut(&leg_id).unwrap().remote_registered = remote;
        while events.try_recv().is_ok() {}
        session
            .execute_command(
                CallCommand::LegRinging {
                    leg_id: leg_id.clone(),
                },
                None,
            )
            .await;
        session.update_leg_state(&leg_id, LegState::Connected);
        for direction in ["sendonly", "sendrecv"] {
            let sdp = format!(
                "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na={direction}\r\n"
            );
            let offer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &sdp).unwrap();
            session
                .apply_reinvite_hold_transition(DialogSide::Callee, &leg_id, &offer, &[])
                .await;
        }
        session.emit_rwi_leg_hangup(&leg_id, Some("UasBye".into()));
        let mut delivered = Vec::new();
        while let Ok(entry) = events.try_recv() {
            if entry.event.payload["leg_id"] == name {
                delivered.push(entry.event);
            }
        }
        // Home-proxy mirror legs (`remote_registered`) stay silent on
        // hold/unheld — the dialog's registration node owns the hold stream
        // (prd 2026-11-23: three-way double `call_held` suppression).
        let (present_kinds, absent_kinds): (&[&str], &[&str]) = if expected.is_some() {
            (
                &[
                    "call_ringing",
                    "call_answered",
                    "call_held",
                    "call_unheld",
                    "call_hangup",
                ],
                &[],
            )
        } else {
            (
                &["call_ringing", "call_answered", "call_hangup"],
                &["call_held", "call_unheld"],
            )
        };
        for kind in present_kinds {
            let event = delivered
                .iter()
                .find(|event| event.event_type == *kind)
                .unwrap_or_else(|| panic!("missing {kind} for {name}: {delivered:?}"));
            assert_eq!(
                event.payload.get("leg_role").and_then(|role| role.as_str()),
                expected,
                "{name}: {event:?}"
            );
        }
        for kind in absent_kinds {
            assert!(
                delivered.iter().all(|event| event.event_type != *kind),
                "{name}: {kind} must not be emitted for a home-proxy mirror leg: {delivered:?}"
            );
        }
    }
    session.cleanup().await;
}

/// CTI consult (`CallCommand::LegAdd` with consult purpose — the
/// owner-anchored `/consult` flow) stashes the transferring agent as the
/// call's `transfer_source` before the dial mutates the session: the
/// consult leg's events carry BOTH their own `leg_role=consult` AND the
/// transfer origin, so consumers tell a consult leg apart from a blind
/// transfer target / ordinary dial on any event.
#[tokio::test]
async fn consult_leg_add_stashes_transfer_source_on_call_events() {
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let gw = Arc::new(parking_lot::RwLock::new(gateway));
    let (server, _) =
        create_test_server_with_rwi_gateway(Default::default(), gw.clone()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "customer",
        None,
        "example.test",
        None,
    );
    let dialplan = Dialplan::new("consult-stash".into(), request, DialDirection::Inbound);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;
    session.extensions.set_agent_attribution("agent-9");

    let consult = LegId::from("consult-tx");
    let added = session
        .execute_command(
            CallCommand::LegAdd {
                source_leg: Some(LegId::from("callee")),
                target: "sip:expert@127.0.0.1:5099".into(),
                leg_id: Some(consult.clone()),
                headers: vec![],
                purpose: Some(crate::call::domain::LegPurpose::Consult),
            },
            None,
        )
        .await;
    assert!(added.success, "{added:?}");
    while events.try_recv().is_ok() {}
    session
        .execute_command(
            CallCommand::LegRinging { leg_id: consult.clone() },
            None,
        )
        .await;
    session.update_leg_state(&consult, LegState::Connected);
    session.emit_rwi_leg_hangup(&consult, Some("UasBye".into()));
    session.cleanup().await;

    let mut delivered = Vec::new();
    while let Ok(entry) = events.try_recv() {
        if entry.event.payload["leg_id"] == "consult-tx" {
            delivered.push(entry.event);
        }
    }
    for kind in ["call_ringing", "call_answered", "call_hangup"] {
        let event = delivered
            .iter()
            .find(|event| event.event_type == kind)
            .unwrap_or_else(|| panic!("missing {kind} for consult-tx: {delivered:?}"));
        assert_eq!(
            event.payload.get("leg_role").and_then(|role| role.as_str()),
            Some("consult"),
            "leg_role contract unchanged: {event:?}"
        );
        assert_eq!(
            event.payload["transfer_source"]["source_type"], "agent",
            "{kind} must carry the transfer origin: {event:?}"
        );
        assert_eq!(
            event.payload["transfer_source"]["agent_id"], "agent-9",
            "{kind}: the transferring agent is the origin: {event:?}"
        );
    }
}

/// Keep-first: a consult `LegAdd` must NOT degrade an origin an earlier
/// transfer step captured before the session mutated (the blind-transfer
/// path deliberately snapshots pre-mutation).
#[tokio::test]
async fn consult_leg_add_keeps_existing_transfer_source() {
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let gw = Arc::new(parking_lot::RwLock::new(gateway));
    let (server, _) =
        create_test_server_with_rwi_gateway(Default::default(), gw.clone()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "customer",
        None,
        "example.test",
        None,
    );
    let dialplan = Dialplan::new("consult-keepfirst".into(), request, DialDirection::Inbound);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;
    session.extensions.set_agent_attribution("agent-9");
    let call_id = session.context.session_id.to_string();
    gw.read().meta_store.insert(
        call_id.clone(),
        crate::rwi::proto::CallMeta {
            transfer_source: Some(crate::rwi::TransferSource {
                source_type: "queue".into(),
                name: Some("support".into()),
                ivr_node_id: None,
                agent_id: Some("agent-7".into()),
                agent_name: None,
            }),
            ..Default::default()
        },
    );

    let added = session
        .execute_command(
            CallCommand::LegAdd {
                source_leg: Some(LegId::from("callee")),
                target: "sip:expert@127.0.0.1:5099".into(),
                leg_id: Some(LegId::from("consult-tx2")),
                headers: vec![],
                purpose: Some(crate::call::domain::LegPurpose::Consult),
            },
            None,
        )
        .await;
    assert!(added.success, "{added:?}");
    let stashed = gw
        .read()
        .meta_store
        .get_sync(&call_id)
        .and_then(|m| m.transfer_source);
    let stashed = stashed.expect("origin must survive the consult dial");
    assert_eq!(stashed.source_type, "queue", "pre-mutation origin wins");
    assert_eq!(stashed.agent_id.as_deref(), Some("agent-7"));
}


/// A synchronous dial deadline cancels only the unanswered target, even after ringing.
#[tokio::test]
async fn awaited_transfer_connect_timeout_preserves_caller() {
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::udp::UdpConnection;

    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = target.local_addr().unwrap();
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let endpoint = server.endpoint.inner.clone();
    for local in endpoint.transport_layer.get_addrs() {
        endpoint.transport_layer.del_transport(&local);
    }
    let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
        .await
        .unwrap();
    endpoint.transport_layer.add_transport(transport.into());
    let serving_endpoint = endpoint.clone();
    let serving = tokio::spawn(async move { serving_endpoint.serve().await.unwrap() });
    let request = create_test_request(Method::Invite, "customer", None, "example.test", None);
    let dialplan = Dialplan::new("timed-transfer".into(), request, DialDirection::Inbound)
        .with_caller("sip:customer@example.test".parse().unwrap())
        .with_media(crate::call::MediaConfig::new().with_proxy_mode(crate::config::MediaProxyMode::All));
    let (mut session, _handle, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;
    setup_recording_test_media(&mut session).await;
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(state_tx);
    let command: CallCommand = serde_json::from_value(serde_json::json!({
        "type": "transfer_await_result",
        "leg_id": "caller",
        "target": format!("sip:target@{address}"),
        "headers": {},
        "timeouts": {"connect_timeout_ms": 3000}
    }))
    .unwrap();
    let responder = async {
        let mut buffer = [0u8; 65536];
        let (size, source) = tokio::time::timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
            .await
            .expect("transfer must issue INVITE")
            .unwrap();
        let SipMessage::Request(invite) =
            SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
        else {
            panic!("expected INVITE");
        };
        assert_eq!(invite.method, Method::Invite);
        let mut ringing = endpoint.make_response(&invite, StatusCode::Ringing, None);
        ringing.headers.retain(|header| !matches!(header, Header::To(_)));
        ringing.headers.push(Header::To(
            format!("<sip:target@{address}>;tag=timed-target").into(),
        ));
        target.send_to(ringing.to_string().as_bytes(), source).await.unwrap();
        loop {
            let event = events.recv().await.unwrap();
            if event.event.event_type == "call_ringing" {
                break;
            }
        }
        // Freeze only after the real provisional response has reached the dial loop.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
    };
    let dial = async {
        tokio::time::timeout(
            Duration::from_secs(4),
            session.execute_command(command, Some(&mut state_rx)),
        )
        .await
        .expect("configured connect timeout must finish the dial before the SIP transaction timeout")
    };
    let (result, ()) = tokio::join!(dial, responder);
    tokio::time::resume();
    assert!(!result.success, "unanswered target must fail the transfer");
    assert!(!session.cancel_token.is_cancelled(), "caller must survive the dial timeout");
    assert!(!session.caller_dialog.as_ref().unwrap().state().is_terminated());
    let cancel = tokio::time::timeout(Duration::from_secs(3), async {
        let mut buffer = [0u8; 65536];
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(request) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
                && request.method == Method::Cancel
            {
                break request;
            }
        }
    })
    .await
    .expect("provisional target must receive CANCEL");
    assert!(cancel.uri.to_string().starts_with("sip:target@"));
    session.cleanup().await;
    serving.abort();
}


/// Segment duration starts at answer and ends only the connected target.
#[tokio::test]
async fn awaited_transfer_max_duration_returns_failure_without_ending_caller() {
    for bye_transport_fails in [false, true] {
        check_transfer_max_duration(bye_transport_fails).await;
    }
}

async fn check_transfer_max_duration(bye_transport_fails: bool) {
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::udp::UdpConnection;

    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = target.local_addr().unwrap();
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let endpoint = server.endpoint.inner.clone();
    for local in endpoint.transport_layer.get_addrs() {
        endpoint.transport_layer.del_transport(&local);
    }
    let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
        .await
        .unwrap();
    endpoint.transport_layer.add_transport(transport.into());
    let serving_endpoint = endpoint.clone();
    let serving = tokio::spawn(async move { serving_endpoint.serve().await.unwrap() });
    let request = create_test_request(Method::Invite, "customer", None, "example.test", None);
    let dialplan = Dialplan::new("timed-transfer".into(), request, DialDirection::Inbound)
        .with_caller("sip:customer@example.test".parse().unwrap())
        .with_media(crate::call::MediaConfig::new().with_proxy_mode(crate::config::MediaProxyMode::All));
    let (mut session, _handle, commands) = build_session_with_cmd_rx_on(server.clone(), dialplan).await;
    // Keep the UAS transaction alive so answering the caller uses a live response channel.
    let caller_request = create_test_request(Method::Invite, "customer", None, "example.test", None);
    let (caller_transaction, _caller_endpoint) =
        crate::proxy::tests::common::create_transaction(caller_request).await;
    let (caller_state_tx, _caller_states) = mpsc::unbounded_channel();
    let caller_dialog = server.dialog_layer
        .get_or_create_server_invite(&caller_transaction, caller_state_tx, None, None)
        .unwrap();
    session.legs.set_dialog(
        LegId::from("caller"),
        rsipstack::dialog::dialog::Dialog::Invite(caller_dialog.clone()),
    );
    session.caller_dialog = Some(caller_dialog);
    setup_recording_test_media(&mut session).await;
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(state_tx);
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    session.app_event_bridge.set_app_event_sender(Some(app_tx));
    let command: CallCommand = serde_json::from_value(serde_json::json!({
        "type": "transfer_await_result",
        "leg_id": "caller",
        "target": format!("sip:target@{address}"),
        "headers": {},
        "timeouts": {"max_duration_ms": 3000}
    }))
    .unwrap();
    let responder = async {
        let mut buffer = [0u8; 65536];
        let (size, source) = tokio::time::timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
            .await
            .expect("transfer must issue INVITE")
            .unwrap();
        let SipMessage::Request(invite) =
            SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
        else {
            panic!("expected INVITE");
        };
        let mut answer = endpoint.make_response(&invite, StatusCode::OK, Some(invite.body.clone()));
        answer.headers.retain(|header| !matches!(header, Header::To(_)));
        answer.headers.push(Header::To(
            format!("<sip:target@{address}>;tag=timed-target").into(),
        ));
        answer.headers.push(Header::Contact(format!("<sip:target@{address}>").into()));
        answer.headers.push(Header::ContentType("application/sdp".into()));
        target.send_to(answer.to_string().as_bytes(), source).await.unwrap();
    };
    let dial = async {
        tokio::time::timeout(
            Duration::from_secs(5),
            session.execute_command(command, Some(&mut state_rx)),
        )
        .await
        .expect("target must answer")
    };
    let (result, ()) = tokio::join!(dial, responder);
    assert!(result.success, "{result:?}");
    let target_dialog = session.meta.connected_callee_dialog_id.clone().unwrap();
    let late_states = session.callee_event_tx.clone().unwrap();
    if bye_transport_fails {
        // Remove the real UDP send path after answer; BYE must fail transport lookup.
        for local in endpoint.transport_layer.get_addrs() {
            endpoint.transport_layer.del_transport(&local);
        }
        let destination = rsipstack::transport::SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: address.to_string().as_str().try_into().unwrap(),
        };
        assert!(endpoint.transport_layer.lookup(&destination, None).await.is_err());
    }
    let cancel = session.cancel_token.clone();
    let observe = async {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        if !bye_transport_fails {
            let (bye, source) = tokio::time::timeout(Duration::from_secs(1), async {
                let mut buffer = [0u8; 65536];
                loop {
                    let (size, source) = target.recv_from(&mut buffer).await.unwrap();
                    if let SipMessage::Request(request) =
                        SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
                        && request.method == Method::Bye
                    {
                        break (request, source);
                    }
                }
            })
            .await
            .expect("segment deadline must send BYE to the target");
            tokio::time::resume();
            let response = endpoint.make_response(&bye, StatusCode::OK, None);
            target.send_to(response.to_string().as_bytes(), source).await.unwrap();
        } else {
            tokio::time::resume();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let crate::call::app::ControllerEvent::TransferResult(outcome) = app_rx.recv().await.unwrap() {
                    break outcome;
                }
            }
        })
        .await
        .expect("target teardown must return the transfer result");
        assert_eq!(serde_json::to_value(outcome).unwrap(), "max_duration");
        assert!(!cancel.is_cancelled(), "segment expiry must preserve the caller");
        late_states.send(DialogState::Terminated(
            target_dialog.clone(), TerminatedReason::Timeout,
        )).unwrap();
        cancel.cancel();
    };
    let (run, ()) = tokio::join!(session.run_main_loop(None, state_rx, commands), observe);
    run.unwrap();
    assert!(!session.caller_dialog.as_ref().unwrap().state().is_terminated());
    assert!(session.meta.pending_transfer.is_none());
    assert!(session.meta.connected_callee_dialog_id.is_none());
    assert!(session.server.dialog_layer.get_dialog(&target_dialog).is_none());
    while let Ok(event) = app_rx.try_recv() {
        assert!(!matches!(event, crate::call::app::ControllerEvent::TransferResult(_)),
            "late target teardown must not deliver a duplicate result");
    }
    session.cleanup().await;
    serving.abort();
}

/// Client-side local 3-way (restsend `consult-state=merged` marker): the
/// terminal-hop session synthesizes `conference_created` +
/// per-leg `conference_member_joined` exactly once (latch), member teardowns
/// emit `conference_member_left` with the latch-time role snapshot, and the
/// session-teardown sweep reports remaining members. Asserted against the
/// real RWI gateway event JSON.
#[tokio::test]
async fn local_three_way_merge_synthesizes_conference_event_trio() {
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        crate::config::ProxyConfig {
            route_originated_calls: false,
            ..Default::default()
        },
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "customer",
        None,
        "example.test",
        None,
    );
    let dialplan = Dialplan::new("local-3way".into(), request, DialDirection::Inbound);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;

    // Two live members at the merge edge: the caller leg and a consult-style
    // dynamic leg (inserted directly — no dial outcome dependency).
    session.update_leg_state(&LegId::from("caller"), crate::call::domain::LegState::Connected);
    let consult = LegId::from("consult-x");
    let mut consult_leg = crate::call::domain::Leg::new(consult.clone());
    consult_leg.purpose = Some(crate::call::domain::LegPurpose::Consult);
    session.legs.insert(consult.clone(), consult_leg);
    session.update_leg_state(&consult, crate::call::domain::LegState::Connected);
    while events.try_recv().is_ok() {}

    // Merge edge: created + one joined per live member.
    session.note_local_three_way_merged();
    let call_id = session.context.session_id.to_string();
    let conf_id = format!("consult-{}", call_id);
    let mut batch = Vec::new();
    while let Ok(entry) = events.try_recv() {
        batch.push(entry.event);
    }
    let created = batch
        .iter()
        .find(|e| e.event_type == "conference_created")
        .unwrap_or_else(|| panic!("missing conference_created: {batch:?}"));
    assert_eq!(created.payload["conf_id"], conf_id);
    assert_eq!(
        created.payload["call_id"], call_id,
        "conference_created must carry the call id (send_to_owner routing): {created:?}"
    );
    let joined: Vec<_> = batch
        .iter()
        .filter(|e| e.event_type == "conference_member_joined")
        .collect();
    assert_eq!(joined.len(), 2, "one member_joined per live leg: {batch:?}");
    for event in &joined {
        assert_eq!(event.payload["conf_id"], conf_id);
        assert_eq!(event.payload["call_id"], call_id);
        assert!(
            event.payload["leg_id"].is_string()
                && event.payload["leg_role"].is_string()
                && event.payload["leg_id"] != serde_json::Value::Null
                && event.payload["leg_role"] != serde_json::Value::Null,
            "synthesized members carry leg identity + role: {event:?}"
        );
    }
    let caller_joined = joined
        .iter()
        .find(|e| e.payload["leg_id"] == "caller")
        .expect("caller leg must be a member");
    assert_eq!(caller_joined.payload["leg_role"], "caller");
    let consult_joined = joined
        .iter()
        .find(|e| e.payload["leg_id"] == "consult-x")
        .expect("consult leg must be a member");
    assert_eq!(consult_joined.payload["leg_role"], "consult");

    // Latch: re-sent merged markers are no-ops.
    session.note_local_three_way_merged();
    session.note_local_three_way_merged();
    assert!(
        events.try_recv().is_err(),
        "latch must suppress duplicate conference events"
    );

    // Member teardown → member_left with the latch-time role snapshot.
    session.emit_rwi_leg_hangup(&consult, Some("UasBye".into()));
    let mut left = Vec::new();
    while let Ok(entry) = events.try_recv() {
        if entry.event.event_type == "conference_member_left" {
            left.push(entry.event);
        }
    }
    assert_eq!(left.len(), 1, "exactly one member_left: {left:?}");
    assert_eq!(left[0].payload["leg_id"], "consult-x");
    assert_eq!(left[0].payload["leg_role"], "consult");
    assert_eq!(left[0].payload["conf_id"], conf_id);
    assert_eq!(left[0].payload["call_id"], call_id);

    // Session teardown sweeps the remaining member (the caller leg).
    session.cleanup().await;
    let mut swept = Vec::new();
    while let Ok(entry) = events.try_recv() {
        if entry.event.event_type == "conference_member_left" {
            swept.push(entry.event);
        }
    }
    assert_eq!(
        swept.len(),
        1,
        "teardown sweep reports the remaining member exactly once: {swept:?}"
    );
    assert_eq!(swept[0].payload["leg_id"], "caller");
    assert_eq!(swept[0].payload["leg_role"], "caller");
}
