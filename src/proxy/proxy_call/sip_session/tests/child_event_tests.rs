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
        for kind in [
            "call_ringing",
            "call_answered",
            "call_held",
            "call_unheld",
            "call_hangup",
        ] {
            let event = delivered
                .iter()
                .find(|event| event.event_type == kind)
                .unwrap_or_else(|| panic!("missing {kind} for {name}: {delivered:?}"));
            assert_eq!(
                event.payload.get("leg_role").and_then(|role| role.as_str()),
                expected,
                "{name}: {event:?}"
            );
        }
    }
    session.cleanup().await;
}
