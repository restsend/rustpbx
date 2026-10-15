use super::*;
use crate::addons::cc::agent::{AgentRegistry, AgentStatus};
use crate::addons::cc::cc_call_session_hook::CcCallSessionHook;
use crate::addons::cc::metrics::MetricsCollector;
use crate::proxy::proxy_call::session_hooks::CallSessionHook;
use crate::proxy::tests::common::create_test_request;
use crate::proxy::tests::test_sip_session_regressions::{
    build_session_with_cmd_rx, setup_recording_test_media,
};
use rsipstack::dialog::dialog::TransactionCommand;

const RECORDING_ID: &str = "11111111-1111-4111-8111-111111111111";
const NEW_RECORDING_ID: &str = "22222222-2222-4222-8222-222222222222";
const AGENT: &str = "agent-example";

struct FailingFinalizeRecorder;

#[async_trait::async_trait]
impl crate::media::media_recorder::MediaRecorder for FailingFinalizeRecorder {
    async fn write_rtp(
        &mut self,
        _channel: crate::media::media_recorder::CaptureChannel,
        _packet: &rustrtc::rtp::RtpPacket,
        _received_at_micros: u64,
        _peer_addr: Option<std::net::SocketAddr>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn finalize(
        self: Box<Self>,
    ) -> anyhow::Result<Option<crate::media::media_recorder::RecordingResult>> {
        Err(anyhow!("storage finalization failed"))
    }
}

fn dialplan() -> crate::call::Dialplan {
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "example.invalid",
        None,
    );
    let dialplan =
        crate::call::Dialplan::new("test-session".into(), request, DialDirection::Inbound)
            .with_media(
                crate::call::MediaConfig::new().with_proxy_mode(crate::config::MediaProxyMode::All),
            );
    dialplan
}

async fn session() -> (SipSession, SipSessionHandle, mpsc::Receiver<CallCommand>) {
    build_session_with_cmd_rx(dialplan()).await
}

async fn start_recording(
    session: &mut SipSession,
    path: &std::path::Path,
    id: &str,
    kind: &str,
    label: &str,
) {
    setup_recording_test_media(session).await;
    let result = session
        .execute_command(
            CallCommand::StartRecording {
                config: crate::call::domain::RecordConfig {
                    path: path.to_string_lossy().into_owned(),
                    segment_type: Some(kind.into()),
                    label: Some(label.into()),
                    unique_id: Some(id.into()),
                    notify_app: Some(false),
                    ..Default::default()
                },
            },
            None,
        )
        .await;
    assert!(result.success, "{result:?}");
}

fn stop_body(id: &str) -> String {
    serde_json::json!({"action":"record.stop_segment", "params":{"label":AGENT,"unique_id":id}})
        .to_string()
}

async fn receive_stop(session: &mut SipSession, body: &str) -> (StatusCode, Option<Vec<u8>>) {
    let (transaction, mut responses) = TransactionHandle::new();
    let mut request = create_test_request(
        rsipstack::sip::Method::Info,
        AGENT,
        None,
        "example.invalid",
        None,
    );
    request.headers.push(rsipstack::sip::Header::ContentType(
        RUSTPBX_COMMAND_CT.into(),
    ));
    request.body = body.as_bytes().to_vec();
    let dialog_id = session.caller_dialog_id();
    session
        .handle_dialog_info(DialogSide::Callee, &dialog_id, request, transaction)
        .await
        .unwrap();
    let TransactionCommand::Respond { status, body, .. } = responses.recv().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn recording_stop_confirms_finalization_even_with_full_command_queue() {
    let (mut owner, handle, _commands) = session().await;
    let dir = tempfile::tempdir().unwrap();
    start_recording(
        &mut owner,
        &dir.path().join("agent.wav"),
        RECORDING_ID,
        "agent",
        AGENT,
    )
    .await;
    for _ in 0..CMD_CHANNEL_CAPACITY {
        handle.send_command(CallCommand::ResumeMedia).unwrap();
    }
    let (status, body) = receive_stop(&mut owner, &stop_body(RECORDING_ID)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !owner
            .media
            .recording
            .recorder_status()
            .await
            .unwrap()
            .active,
        "INFO success must mean the recorder was finalized, not merely queued"
    );
    assert_eq!(owner.record_snapshot().recording_segments.len(), 1);
    let ack: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
    assert_eq!(ack["unique_id"], RECORDING_ID);
    assert_eq!(ack["stopped"], true);
}

#[tokio::test]
async fn recording_stop_preserves_other_id_label_and_recording_types() {
    for (id, kind, label) in [
        (NEW_RECORDING_ID, "agent", AGENT),
        (RECORDING_ID, "agent", "another-agent"),
        (RECORDING_ID, "full", AGENT),
        (RECORDING_ID, "segment", AGENT),
    ] {
        let (mut owner, _handle, mut commands) = session().await;
        let dir = tempfile::tempdir().unwrap();
        start_recording(
            &mut owner,
            &dir.path().join("protected.wav"),
            id,
            kind,
            label,
        )
        .await;
        receive_stop(&mut owner, &stop_body(RECORDING_ID)).await;
        while let Ok(command) = commands.try_recv() {
            owner.execute_command(command, None).await;
        }
        assert!(
            owner
                .media
                .recording
                .recorder_status()
                .await
                .unwrap()
                .active,
            "a stale or foreign recording stop must preserve {kind}/{label}/{id}"
        );
        owner
            .execute_command(CallCommand::StopRecording, None)
            .await;
    }
}

#[tokio::test]
async fn recording_stop_rejects_missing_identity() {
    let (mut owner, _handle, _commands) = session().await;
    for params in [
        serde_json::json!({"label": AGENT}),
        serde_json::json!({"label":AGENT,"unique_id":""}),
    ] {
        let body = serde_json::json!({"action":"record.stop_segment","params":params}).to_string();
        assert_eq!(
            receive_stop(&mut owner, &body).await.0,
            StatusCode::BadRequest
        );
    }
}

#[tokio::test]
async fn recording_stop_never_confirms_a_failed_finalization_on_retry() {
    let (mut owner, _handle, _commands) = session().await;
    let dir = tempfile::tempdir().unwrap();
    start_recording(
        &mut owner,
        &dir.path().join("failed.wav"),
        RECORDING_ID,
        "agent",
        AGENT,
    )
    .await;
    // Replace only the uncontrollable storage boundary; keep the real active segment
    // and recording task, including its destructive finalize/take behavior.
    owner.media.recording.stop_recording().await.unwrap();
    owner
        .media
        .recording
        .set_recorder(Box::new(FailingFinalizeRecorder), None)
        .await
        .unwrap();
    for _ in 0..3 {
        assert_ne!(
            receive_stop(&mut owner, &stop_body(RECORDING_ID)).await.0,
            StatusCode::OK,
            "a removed recorder without a completed artifact must not confirm success"
        );
        assert!(owner.record_snapshot().recording_segments.is_empty());
    }
}

#[tokio::test]
async fn recording_stop_is_consumed_across_real_transfer_context_clones() {
    let (mut downstream, handle, mut commands) = session().await;
    let mut dialplan = dialplan();
    dialplan.extensions.insert(HashMap::from([
        ("cc_recording_delivery".to_string(), "true".to_string()),
        ("cc_call_identity".to_string(), RECORDING_ID.to_string()),
        ("resolved_agent_id".to_string(), AGENT.to_string()),
    ]));
    downstream.context.dialplan = Arc::new(dialplan);
    let agents = Arc::new(AgentRegistry::new());
    agents.register(AGENT.into(), vec![], 1).await.unwrap();
    agents
        .update_status(AGENT, AgentStatus::Idle)
        .await
        .unwrap();
    let registry = downstream.server.active_call_registry.clone();
    registry.register_handle(downstream.context.session_id.clone(), handle);
    let hook = CcCallSessionHook::new(agents, Arc::new(MetricsCollector::new()))
        .with_call_registry_shared(Arc::new(tokio::sync::RwLock::new(Some(registry))));
    hook.on_call_connected(&downstream.session_hook_ctx()).await;
    while commands.try_recv().is_ok() {}
    downstream.meta.transferred = true;
    let mut agent = crate::call::domain::Leg::new(LegId::from("agent"));
    agent.agent_id = Some(AGENT.into());
    agent.state = crate::call::domain::LegState::Connected;
    downstream.legs.insert(LegId::from("agent"), agent);
    for attempt in 0..2 {
        let ctx = downstream
            .transferred_agent_context(&LegId::from("agent"))
            .unwrap();
        hook.on_agent_disconnected(&ctx, &*downstream.app_runtime)
            .await;
        if attempt == 0 {
            let command = tokio::time::timeout(Duration::from_secs(1), commands.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(command, CallCommand::StopPeerRecordingSegment { ref label, ref unique_id }
                if label == AGENT && unique_id == RECORDING_ID)
            );
        } else {
            assert!(
                commands.try_recv().is_err(),
                "recreated transfer contexts must consume the delivered recording only once"
            );
        }
    }
}

#[tokio::test]
async fn recording_stop_retries_real_sip_failures_and_requires_owner_confirmation() {
    use rsipstack::sip::{Header, Method, SipMessage};
    use rsipstack::transport::{SipAddr, udp::UdpConnection};

    for recover in [true, false] {
        let (mut owner, owner_handle, _owner_commands) = session().await;
        let dir = tempfile::tempdir().unwrap();
        start_recording(
            &mut owner,
            &dir.path().join("owner.wav"),
            RECORDING_ID,
            "agent",
            AGENT,
        )
        .await;
        for _ in 0..CMD_CHANNEL_CAPACITY {
            owner_handle.send_command(CallCommand::ResumeMedia).unwrap();
        }
        let owner = Arc::new(tokio::sync::Mutex::new(owner));
        let (server, _) = crate::proxy::tests::common::create_test_server().await;
        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        server
            .endpoint
            .inner
            .transport_layer
            .del_transport(&SipAddr {
                r#type: Some(rsipstack::sip::Transport::Udp),
                addr: "127.0.0.1:5060".try_into().unwrap(),
            });
        let transport =
            UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
                .await
                .unwrap();
        server
            .endpoint
            .inner
            .transport_layer
            .add_transport(transport.into());
        let endpoint = server.endpoint.inner.clone();
        let serving = tokio::spawn(async move {
            endpoint.serve().await.unwrap();
        });
        let (temporary, _, _) = session().await;
        let cancel = CancellationToken::new();
        let _cancel_on_exit = cancel.clone().drop_guard();
        let (mut downstream, handle, mut commands) = SipSession::new_uac(
            server.clone(),
            cancel,
            None,
            temporary.context.clone(),
            true,
        );
        let (callee_tx, _callee_rx) = mpsc::unbounded_channel();
        downstream.callee_event_tx = Some(callee_tx);
        let added = downstream
            .execute_command(
                CallCommand::LegAdd {
                    source_leg: None,
                    target: format!("sip:upstream@{peer_addr}"),
                    leg_id: Some(LegId::from("upstream")),
                    headers: vec![],
                    purpose: None,
                },
                None,
            )
            .await;
        assert!(added.success, "{added:?}");
        let mut buffer = [0u8; 65536];
        let (size, pbx) = tokio::time::timeout(Duration::from_secs(3), peer.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let SipMessage::Request(invite) =
            SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
        else {
            panic!("INVITE");
        };
        let call_id = invite.call_id_header().unwrap().value().to_string();
        let mut answer =
            server
                .endpoint
                .inner
                .make_response(&invite, StatusCode::OK, Some(invite.body.clone()));
        answer
            .headers
            .retain(|header| !matches!(header, Header::To(_)));
        answer.headers.push(Header::To(
            format!("<sip:upstream@{peer_addr}>;tag=recording-peer").into(),
        ));
        answer.headers.push(Header::Contact(
            format!("<sip:upstream@{peer_addr}>").into(),
        ));
        answer
            .headers
            .push(Header::ContentType("application/sdp".into()));
        peer.send_to(answer.to_string().as_bytes(), pbx)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(command) = commands.recv().await {
                if matches!(command, CallCommand::LegConnected { .. }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        downstream.caller_dialog =
            Some(server.dialog_layer.get_client_dialog_by_call_id(&call_id)[0].clone());

        let complete = Arc::new(tokio::sync::Notify::new());
        let notification = complete.clone();
        let recorder = owner.clone();
        let endpoint = server.endpoint.inner.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let receiver = tokio::spawn(async move {
            loop {
                let (size, source) = peer.recv_from(&mut buffer).await.unwrap();
                let SipMessage::Request(request) =
                    SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
                else {
                    continue;
                };
                if request.method != Method::Info {
                    continue;
                }
                let attempt = observed.fetch_add(1, Ordering::SeqCst) + 1;
                let (status, body) = if recover && attempt >= 3 {
                    let mut owner = recorder.lock().await;
                    receive_stop(&mut owner, std::str::from_utf8(&request.body).unwrap()).await
                } else if recover && attempt == 2 {
                    (StatusCode::ServiceUnavailable, None)
                } else {
                    // An old peer acknowledges unknown commands without executing them.
                    (StatusCode::OK, None)
                };
                let response = endpoint.make_response(&request, status, body);
                peer.send_to(response.to_string().as_bytes(), source)
                    .await
                    .unwrap();
                if attempt >= 3 {
                    notification.notify_one();
                }
            }
        });
        let mut routed = dialplan();
        routed.extensions.insert(HashMap::from([
            ("cc_recording_delivery".to_string(), "true".to_string()),
            ("cc_call_identity".to_string(), RECORDING_ID.to_string()),
            ("resolved_agent_id".to_string(), AGENT.to_string()),
        ]));
        downstream.context.dialplan = Arc::new(routed);
        let agents = Arc::new(AgentRegistry::new());
        agents.register(AGENT.into(), vec![], 1).await.unwrap();
        agents
            .update_status(AGENT, AgentStatus::Idle)
            .await
            .unwrap();
        let registry = server.active_call_registry.clone();
        registry.register_handle(downstream.context.session_id.clone(), handle);
        let hook = CcCallSessionHook::new(agents, Arc::new(MetricsCollector::new()))
            .with_call_registry_shared(Arc::new(tokio::sync::RwLock::new(Some(registry))));
        hook.on_call_connected(&downstream.session_hook_ctx()).await;
        hook.stop_agent_recording(&downstream.session_hook_ctx(), AGENT)
            .await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let command = commands.recv().await.unwrap();
                    let stop = matches!(command, CallCommand::StopPeerRecordingSegment { .. });
                    let result = downstream.execute_command(command, None).await;
                    if stop { break result; }
                }
            })
            .await
            .expect("network retries must not block the session loop")
            .success
        );
        tokio::time::timeout(Duration::from_secs(3), complete.notified())
            .await
            .expect("missing confirmation must trigger bounded retries");
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert_eq!(
            owner
                .lock()
                .await
                .media
                .recording
                .recorder_status()
                .await
                .unwrap()
                .active,
            !recover
        );
        // An extra request after the third response would violate the retry budget.
        assert!(
            tokio::time::timeout(Duration::from_millis(400), complete.notified())
                .await
                .is_err()
        );
        owner
            .lock()
            .await
            .execute_command(CallCommand::StopRecording, None)
            .await;
        receiver.abort();
        serving.abort();
    }
}

/// A `record.stop_segment` for an identity this node never recorded must
/// answer 404 AND surface the miss in the call trace — the 404 is the
/// smoking gun for "the promised agent recording never started upstream"
/// (2026-10-14 incident) and used to be completely silent, on the wire and
/// in the CDR.
#[tokio::test]
async fn recording_stop_unknown_identity_answers_404_and_traces_it() {
    let (mut owner, _handle, _commands) = session().await;
    let body = serde_json::json!({
        "action": "record.stop_segment",
        "params": {"label": AGENT, "unique_id": "00000000-0000-4000-8000-000000000000"}
    })
    .to_string();
    let (status, body) = receive_stop(&mut owner, &body).await;
    assert_eq!(status, StatusCode::NotFound);
    assert!(body.is_none(), "a 404 carries no ack body");
    assert!(
        owner.meta.trace.iter().any(|event| {
            event.code.as_deref()
                == Some(crate::callrecord::recording_upload::recording_trace_codes::PEER_SEGMENT_STOP_NOT_FOUND)
        }),
        "the unknown-identity stop must be visible in the call trace"
    );
}
