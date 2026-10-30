use super::*;
use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry};
use crate::call::{DialDirection, DialStrategy, Dialplan, Location, SipUser, TransactionCookie};
use crate::proxy::tests::common::create_test_server_with_rwi_gateway;
use rsipstack::sip::{Header, Method, SipMessage};
use rsipstack::transport::udp::UdpConnection;

// Exercise serve -> process -> setup failure -> cleanup, including the actual
// caller dialog. Never pre-stamp its role or manufacture an Ended leg.
async fn outbound_failure(
    from: &str,
    auth_user: Option<&str>,
    contact: &str,
    status: u16,
) -> Vec<crate::rwi::EventCacheEntry> {
    let gateway = Arc::new(parking_lot::RwLock::new(crate::rwi::RwiGateway::new()));
    let mut events = gateway.read().subscribe_events();
    let (mut server, _) = create_test_server_with_rwi_gateway(Default::default(), gateway).await;
    let registry = Arc::new(MemoryRegistry::new());
    for agent in ["agent-a", "agent-b"] {
        registry
            .register(
                agent.into(),
                agent.into(),
                format!("sip:{agent}@rustpbx.com"),
                vec![],
                1,
            )
            .await
            .unwrap();
    }
    Arc::get_mut(&mut server).unwrap().agent_registry = Some(registry);
    let endpoint = server.endpoint.inner.clone();
    for addr in endpoint.transport_layer.get_addrs() {
        endpoint.transport_layer.del_transport(&addr);
    }
    let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
        .await
        .unwrap();
    let pbx = transport.get_addr().addr.to_string();
    endpoint.transport_layer.add_transport(transport.into());
    let mut incoming = server.endpoint.incoming_transactions().unwrap();
    let serving_endpoint = endpoint.clone();
    let serving = tokio::spawn(async move { serving_endpoint.serve().await.unwrap() });

    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller_addr = caller.local_addr().unwrap();
    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    let call_id = uuid::Uuid::new_v4().to_string();
    let session_id = call_id.clone();
    let auth_user = auth_user.map(str::to_owned);
    let (records, mut received) = mpsc::channel(1);
    let processing = tokio::spawn(async move {
        let mut tx = incoming.recv().await.unwrap();
        assert_eq!(tx.original.method, Method::Invite);
        let original = tx.original.clone();
        let original_caller = original.from_header().unwrap().uri().unwrap().to_string();
        let mut dialplan = Dialplan::new(session_id.clone(), original, DialDirection::Outbound)
            .with_caller(original_caller.parse().unwrap())
            .with_targets(DialStrategy::Sequential(vec![Location {
                aor: format!("sip:customer@{target_addr}").parse().unwrap(),
                ..Default::default()
            }
            .into()]))
            .with_media(
                crate::call::MediaConfig::new()
                    .with_proxy_mode(crate::config::MediaProxyMode::None),
            );
        dialplan.max_ring_time = Some(Duration::from_millis(if status == 408 {
            500
        } else {
            5000
        }));
        let cookie = TransactionCookie::default();
        if let Some(username) = auth_user {
            // The validated auth boundary supplies this identity independently
            // of the original From, as JwtAuthBackend does in production.
            cookie.set_user(SipUser {
                username,
                ..Default::default()
            });
        }
        let context = CallContext {
            session_id,
            dialplan: Arc::new(dialplan),
            cookie,
            start_time: Instant::now(),
            original_caller,
            original_callee: format!("sip:customer@{pbx}"),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        SipSession::serve(
            server,
            context,
            &mut tx,
            CancellationToken::new(),
            Some(records),
        )
        .await
        .unwrap();
    });
    let pbx = endpoint.transport_layer.get_addrs()[0].addr.to_string();
    let to = format!("<sip:customer@{pbx}>");
    let request = |method: &str, to: &str| {
        format!(
            "{method} sip:customer@{pbx} SIP/2.0\r\nVia: SIP/2.0/UDP {caller_addr};branch=z9hG4bK{call_id}\r\nFrom: <sip:{from}@rustpbx.com>;tag=caller\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: 1 {method}\r\nContact: <sip:{contact}@{caller_addr}>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )
    };
    caller
        .send_to(request("INVITE", &to).as_bytes(), &pbx)
        .await
        .unwrap();
    let mut buffer = [0u8; 65536];
    let (size, source) =
        tokio::time::timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
    let SipMessage::Request(invite) =
        SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
    else {
        panic!("expected downstream INVITE")
    };
    assert_eq!(invite.method, Method::Invite);
    let mut response = endpoint.make_response(&invite, StatusCode::Ringing, None);
    response.headers.retain(|h| !matches!(h, Header::To(_)));
    response.headers.push(Header::To(
        format!("<sip:customer@{target_addr}>;tag=target").into(),
    ));
    target
        .send_to(response.to_string().as_bytes(), source)
        .await
        .unwrap();
    if status == 487 {
        caller
            .send_to(request("CANCEL", &to).as_bytes(), &pbx)
            .await
            .unwrap();
    } else if status != 408 {
        response.status_code = StatusCode::Other(status, "Rejected".into());
        target
            .send_to(response.to_string().as_bytes(), source)
            .await
            .unwrap();
    }
    // ACK the actual final INVITE response so the real UAS can finish.
    let final_code = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (size, _) = caller.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Response(response) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
            {
                if response.status_code.code() >= 300
                    && response.cseq_header().unwrap().typed().unwrap().method == Method::Invite
                {
                    caller
                        .send_to(
                            request("ACK", response.to_header().unwrap().value()).as_bytes(),
                            &pbx,
                        )
                        .await
                        .unwrap();
                    break response.status_code.code();
                }
            }
        }
    })
    .await
    .expect("caller must receive a final failure response");
    assert_eq!(final_code, status);
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            let finished =
                event.event.event_type == "call_hangup" && event.event.payload["scope"] == "call";
            observed.push(event);
            if finished {
                break;
            }
        }
    })
    .await
    .expect("terminal cleanup must emit the session hangup");
    let record = tokio::time::timeout(Duration::from_secs(3), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.call_id, call_id);
    assert!(
        record.answer_time.is_none(),
        "failed setup must remain unanswered"
    );
    assert!(
        record.recorder.is_empty(),
        "unanswered calls with recording disabled create no artifacts"
    );
    assert!(
        observed
            .iter()
            .all(|entry| entry.event.payload["call_id"] == call_id)
    );
    assert!(
        observed
            .iter()
            .all(|entry| entry.event.event_type != "call_answered")
    );
    tokio::time::timeout(Duration::from_secs(5), processing)
        .await
        .unwrap()
        .unwrap();
    serving.abort();
    observed
}

#[tokio::test]
async fn outbound_failure_reports_agent_hangup_from_real_setup() {
    let mut cases = Vec::new();
    for status in [404, 486, 408, 487] {
        cases.push((
            status,
            outbound_failure("agent-a", Some("agent-a"), "agent-a", status).await,
        ));
    }
    for (status, events) in cases {
        let hangups: Vec<_> = events
            .iter()
            .filter(|e| e.event.event_type == "call_hangup")
            .collect();
        assert_eq!(hangups.len(), 2, "status={status}: {hangups:?}");
        assert_eq!(hangups[0].event.payload["scope"], "leg");
        assert_eq!(hangups[0].event.payload["leg_id"], "caller");
        assert_eq!(hangups[0].event.payload["leg_role"], "agent");
        assert_eq!(hangups[0].event.payload["agent_id"], "agent-a");
        assert_eq!(hangups[0].event.payload["duration_secs"], 0);
        assert_eq!(hangups[1].event.payload["scope"], "call");
    }
}

#[tokio::test]
async fn outbound_failure_created_uses_the_resolved_originating_agent() {
    for (from, auth, contact) in [
        ("hotline", Some("agent-a"), "agent-a"),
        ("agent-b", Some("agent-a"), "agent-b"),
        ("hotline", None, "agent-a"),
    ] {
        let events = outbound_failure(from, auth, contact, 486).await;
        let created = events
            .iter()
            .find(|e| e.event.event_type == "call_created")
            .unwrap();
        assert_eq!(
            created.event.payload["leg_role"], "agent",
            "from={from}, auth={auth:?}"
        );
        assert_eq!(created.event.payload["agent_id"], "agent-a");
        let hangups: Vec<_> = events
            .iter()
            .filter(|e| e.event.event_type == "call_hangup")
            .collect();
        assert_eq!(hangups.len(), 2);
        assert_eq!(hangups[0].event.payload["leg_role"], "agent");
        assert_eq!(hangups[0].event.payload["agent_id"], "agent-a");
    }
}

#[tokio::test]
async fn outbound_failure_non_agent_stays_session_scoped() {
    let events = outbound_failure("external", None, "external", 486).await;
    let created = events
        .iter()
        .find(|e| e.event.event_type == "call_created")
        .unwrap();
    assert_eq!(created.event.payload["leg_role"], "caller");
    let hangups: Vec<_> = events
        .iter()
        .filter(|e| e.event.event_type == "call_hangup")
        .collect();
    assert_eq!(hangups.len(), 1);
    assert_eq!(hangups[0].event.payload["scope"], "call");
    assert!(hangups[0].event.payload.get("leg_role").is_none());
}
