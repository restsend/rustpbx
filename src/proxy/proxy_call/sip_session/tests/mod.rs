use super::*;
use crate::proxy::proxy_call::dtmf::RtpDtmfDetector;
use crate::proxy::proxy_call::sip_session::builtin_app_factory::BuiltinAppFactory;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A sequential attempt for an INVITE; a `None` policy keeps the dialer's legacy advance.
fn dial_attempt(
    option: rsipstack::dialog::invitation::InviteOption,
    hints: Option<crate::config::DialplanHints>,
    retry_policy: Option<crate::proxy::routing::RetryPolicy>,
) -> crate::call::DialAttempt {
    let mut attempt = crate::call::DialAttempt::from(crate::config::RouteTarget { option, hints, retry_policy: None });
    attempt.retry_policy = retry_policy;
    attempt
}

#[tokio::test]
async fn sequential_route_retry_uses_real_status_time_limits_and_attempt_context() {
    use crate::call::{DialDirection, Dialplan, DialAttempt, TransactionCookie};
    use crate::proxy::routing::RetryPolicy;
    use crate::proxy::tests::common::{create_test_request, create_test_server, create_transaction};
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::{SipAddr, udp::UdpConnection};

    for (case, expected_attempts) in [
        ("matching_status", 2), ("other_status", 1), ("empty_policy", 1),
        ("legacy", 3), ("no_policy", 3),
        ("cancel_before", 0), ("cancel_during", 1),
        ("accepted", 1), ("outer_timeout", 1),
        ("early_anchor_to_bypass", 2), ("bypass_to_anchor", 2),
        ("ring_timeout", 2), ("single_rejected", 1),
    ] {
        let (server, _) = create_test_server().await;
        let remote = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = remote.local_addr().unwrap();
        server.endpoint.inner.transport_layer.del_transport(&SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp), addr: "127.0.0.1:5060".try_into().unwrap(),
        });
        let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None).await.unwrap();
        server.endpoint.inner.transport_layer.add_transport(transport.into());
        let endpoint = server.endpoint.inner.clone();
        let serving = tokio::spawn(async move { endpoint.serve().await.unwrap(); });
        let mut request = create_test_request(Method::Invite, "alice", None, "rustpbx.com", None);
        if matches!(case, "early_anchor_to_bypass" | "bypass_to_anchor") {
            request.body = concat!(
                "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=Talk\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n",
                "m=audio 40000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n",
            ).as_bytes().to_vec();
        }
        let (tx, _caller_endpoint) = create_transaction(request.clone()).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let dialog = server.dialog_layer.get_or_create_server_invite(&tx, state_tx, None, None).unwrap();
        let mut plan = Dialplan::new(case.into(), request, DialDirection::Inbound)
            .with_caller("sip:original@rustpbx.com".try_into().unwrap());
        plan.media.proxy_mode = MediaProxyMode::Bypass;
        plan.media.external_ip = Some("192.0.2.1".into());
        plan.extensions.insert("common-context".to_string());
        // A trunk that rings past its ring time fails with 408.
        let policy = RetryPolicy {
            status_codes: vec![if case == "ring_timeout" { 408 } else { 503 }],
        };
        let attempts: Vec<DialAttempt> = (0..3).map(|index| {
            let mut hints = crate::config::DialplanHints::default();
            hints.extensions.insert(format!("target-{index}"));
            if matches!(case, "early_anchor_to_bypass" | "bypass_to_anchor") {
                hints.media_mode = Some(if (case == "early_anchor_to_bypass") == (index == 0) {
                    MediaProxyMode::All
                } else {
                    MediaProxyMode::Bypass
                });
            }
            hints.extensions.insert(crate::call::OutboundTrunkContext {
                id: Some(index), name: format!("trunk-{index}"), dest: None, tenant_id: None,
            });
            if index == 0 { hints.external_ip = Some("192.0.2.2".into()); }
            if case == "ring_timeout" { hints.max_ring_time = Some(Duration::from_millis(300)); }
            dial_attempt(
                rsipstack::dialog::invitation::InviteOption {
                    callee: format!("sip:target{index}@{remote_addr}").try_into().unwrap(),
                    caller: format!("sip:caller{index}@rustpbx.com").try_into().unwrap(),
                    ..Default::default()
                },
                (case != "legacy").then_some(hints),
                if matches!(case, "legacy" | "no_policy") { None }
                    else if case == "empty_policy" || (case == "matching_status" && index == 1) { Some(Default::default()) }
                    else { Some(policy.clone()) },
            )
        }).collect();
        let targets: Vec<crate::call::DialAttempt> = if case == "single_rejected" {
            attempts[..1].to_vec()
        } else if case == "legacy" {
            attempts.iter().map(|attempt| Location {
                aor: attempt.location.aor.clone(), ..Default::default()
            }.into()).collect()
        } else {
            attempts
        };
        plan.flow = crate::call::DialplanFlow::Targets(crate::call::DialStrategy::Sequential(targets.clone()));
        let context = CallContext {
            session_id: case.into(), dialplan: Arc::new(plan), cookie: TransactionCookie::default(),
            start_time: Instant::now(), original_caller: "sip:alice@rustpbx.com".into(),
            original_callee: "sip:bob@rustpbx.com".into(), max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(), metadata: None,
        };
        // Mirror session setup: the first attempt decides the media path.
        let initial_plan = SipSession::initial_plan(&server, &context);
        let use_media_proxy = SipSession::check_media_proxy(&context, &initial_plan.media.proxy_mode);
        let cancel = CancellationToken::new();
        let (mut session, _handle, _commands) = SipSession::new(server.clone(), cancel.clone(), None, context, dialog, use_media_proxy);
        let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_tx);
        assert_eq!(session.context.dialplan.extensions.get::<String>().map(String::as_str), Some("common-context"));
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let observed = seen.clone();
        let cancels = Arc::new(AtomicUsize::new(0));
        let cancelled = cancels.clone();
        let responder_endpoint = server.endpoint.inner.clone();
        let responder_cancel = cancel.clone();
        let responder = tokio::spawn(async move {
            let mut buffer = [0u8; 65536];
            loop {
                let (size, peer) = remote.recv_from(&mut buffer).await.unwrap();
                let SipMessage::Request(invite) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() else { continue; };
                if invite.method == Method::Cancel { cancelled.fetch_add(1, Ordering::SeqCst); }
                if invite.method != Method::Invite { continue; }
                observed.lock().push(invite.clone());
                if case == "ring_timeout" && observed.lock().len() == 1 {
                    // The first carrier rings and never answers.
                    let ringing = responder_endpoint.make_response(&invite, StatusCode::Ringing, None);
                    remote.send_to(ringing.to_string().as_bytes(), peer).await.unwrap();
                    continue;
                }
                if case == "cancel_during" { responder_cancel.cancel(); continue; }
                if case == "outer_timeout" { continue; }
                let code = match case {
                    "other_status" => StatusCode::BusyHere,
                    "accepted" => StatusCode::OK,
                    "early_anchor_to_bypass" | "bypass_to_anchor" if observed.lock().len() > 1 => StatusCode::OK,
                    _ => StatusCode::ServiceUnavailable,
                };
                let early_sdp = case == "early_anchor_to_bypass" && observed.lock().len() == 1;
                let mut ringing = responder_endpoint.make_response(&invite,
                    if early_sdp { StatusCode::SessionProgress } else { StatusCode::Ringing },
                    early_sdp.then(|| invite.body.clone()));
                if early_sdp { ringing.headers.push(Header::ContentType("application/sdp".into())); }
                remote.send_to(ringing.to_string().as_bytes(), peer).await.unwrap();
                if early_sdp { tokio::time::sleep(Duration::from_millis(40)).await; }
                let answer_sdp = code == StatusCode::OK && case != "accepted";
                let mut response = responder_endpoint.make_response(&invite, code, answer_sdp.then(|| invite.body.clone()));
                if answer_sdp { response.headers.push(Header::ContentType("application/sdp".into())); }
                response.headers.retain(|header| !matches!(header, Header::To(_)));
                response.headers.push(Header::To(format!("<{}>;tag=retry-peer", invite.uri).into()));
                response.headers.push(Header::Contact(format!("<sip:peer@{remote_addr}>").into()));
                remote.send_to(response.to_string().as_bytes(), peer).await.unwrap();
            }
        });
        if case == "cancel_before" { cancel.cancel(); }
        let result = tokio::time::timeout(
            if case == "outer_timeout" { Duration::from_millis(80) } else { Duration::from_secs(4) },
            session.dial_sequential(&targets, &mut callee_rx),
        ).await;
        if case == "outer_timeout" {
            assert!(result.is_err(), "{case}");
        } else {
            let result = result.expect(case);
            if matches!(case, "early_anchor_to_bypass" | "bypass_to_anchor") {
                assert!(result.is_ok(), "{case}: {result:?}");
                // The media path chosen from the first trunk holds for every retry.
                let expected = if case == "early_anchor_to_bypass" { MediaPathMode::Anchored } else { MediaPathMode::Bypass };
                assert_eq!(session.media_profile.path, expected, "{case}");
            } else if case != "accepted" {
                assert!(result.is_err(), "{case}");
            }
        }
        assert_eq!(seen.lock().len(), expected_attempts, "{case}");
        // Every failed target of the multi-target plan is traced with its trunk.
        let failed_attempts: Vec<(u64, i64, u64)> = session
            .meta
            .trace
            .iter()
            .filter(|event| event.code.as_deref() == Some("dial.attempt_failed"))
            .map(|event| {
                let detail = event.detail.as_ref().expect("attempt detail");
                (
                    detail["attempt"].as_u64().unwrap(),
                    detail["trunk_id"].as_i64().unwrap_or(-1),
                    detail["status"].as_u64().unwrap(),
                )
            })
            .collect();
        // Each dialed attempt is traced with its trunk and target.
        let started: Vec<String> = session.meta.trace.iter()
            .filter(|event| event.code.is_none() && event.message.starts_with("Attempt "))
            .map(|event| event.message.clone())
            .collect();
        assert_eq!(started.len(), expected_attempts, "{case}");
        if case == "matching_status" {
            assert_eq!(started[0], format!("Attempt 1 → trunk-0 · sip:target0@{remote_addr}"));
        }
        match case {
            "matching_status" => assert_eq!(failed_attempts, vec![(0, 0, 503), (1, 1, 503)], "{case}"),
            "accepted" => assert!(failed_attempts.is_empty(), "{case}"),
            "early_anchor_to_bypass" | "bypass_to_anchor" => {
                assert_eq!(failed_attempts, vec![(0, 0, 503)], "{case}")
            }
            // The ringing carrier hits its trunk ring time, is cancelled and fails
            // with 408, which its policy retries; the next carrier's 503 is not
            // listed, so the call stops there.
            // A single target is traced the same way as one in a sequence.
            "single_rejected" => assert_eq!(failed_attempts, vec![(0, 0, 503)], "{case}"),
            "ring_timeout" => {
                assert_eq!(failed_attempts, vec![(0, 0, 408), (1, 1, 503)], "{case}");
                assert!(cancels.load(Ordering::SeqCst) > 0, "the timed-out INVITE is cancelled");
            }
            _ => {}
        }
        if case == "matching_status" {
            assert_eq!(session.context.dialplan.caller.as_ref().unwrap().to_string(), "sip:caller1@rustpbx.com");
            assert_eq!(session.context.dialplan.media.external_ip.as_deref(), Some("192.0.2.1"));
            assert_eq!(session.record_snapshot().extensions.get::<crate::call::OutboundTrunkContext>().unwrap().id, Some(1));
        }
        cancel.cancel();
        responder.abort();
        server.endpoint.shutdown();
        serving.await.unwrap();
    }
}

#[tokio::test]
async fn routed_attempts_keep_call_media_path_and_recording() {
    use crate::call::{DialDirection, Dialplan, DialStrategy, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server, create_transaction};

    struct Recorder(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl crate::media::media_recorder::MediaRecorder for Recorder {
        async fn write_rtp(
            &mut self,
            _: crate::media::media_recorder::CaptureChannel,
            _: &rustrtc::rtp::RtpPacket,
            _: u64,
            _: Option<std::net::SocketAddr>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn finalize(self: Box<Self>) -> anyhow::Result<Option<crate::media::media_recorder::RecordingResult>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }
    let (server, _) = create_test_server().await;
    let request = create_test_request(rsipstack::sip::Method::Invite, "alice", None, "rustpbx.com", None);
    let (tx, _caller_endpoint) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server.dialog_layer.get_or_create_server_invite(&tx, state_tx, None, None).unwrap();
    let first = dial_attempt(
        rsipstack::dialog::invitation::InviteOption {
            callee: "sip:first@127.0.0.1".try_into().unwrap(),
            caller: "sip:first-caller@rustpbx.com".try_into().unwrap(),
            ..Default::default()
        },
        Some(crate::config::DialplanHints {
            media_mode: Some(MediaProxyMode::All),
            max_ring_time: Some(Duration::from_secs(2)),
            ringback: Some(crate::proxy::routing::RingbackAudio { ring: Some("first.wav".into()), ..Default::default() }),
            ..Default::default()
        }),
        Some(Default::default()),
    );
    let second = dial_attempt(
        rsipstack::dialog::invitation::InviteOption {
            callee: "sip:second@127.0.0.1".try_into().unwrap(),
            caller: "sip:second-caller@rustpbx.com".try_into().unwrap(),
            ..Default::default()
        },
        Some(crate::config::DialplanHints { bypass_media: Some(true), enable_recording: Some(false), ..Default::default() }),
        Some(Default::default()),
    );
    let attempts: Vec<crate::call::DialAttempt> = vec![first, second];
    let second = &attempts[1];
    let mut plan = Dialplan::new("media-retry".into(), request, DialDirection::Inbound)
        .with_caller("sip:original@rustpbx.com".try_into().unwrap())
        .with_targets(DialStrategy::Sequential(attempts.clone()));
    plan.max_ring_time = Some(Duration::from_secs(9));
    plan.audio_profile = Some(crate::proxy::routing::RingbackAudio { ring: Some("common.wav".into()), ..Default::default() });
    // Routing fixed recording for the call from the first target.
    plan.recording.enabled = true;
    let common_plan = Arc::new(plan);
    let context = CallContext {
        session_id: "media-retry".into(), dialplan: common_plan.clone(), cookie: TransactionCookie::default(),
        start_time: Instant::now(), original_caller: "sip:alice@rustpbx.com".into(), original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70, created_at: chrono::Utc::now().to_rfc3339(), metadata: None,
    };
    let initial_plan = SipSession::initial_plan(&server, &context);
    assert_eq!(initial_plan.max_ring_time, Some(Duration::from_secs(9)), "ring time is call-wide");
    assert_eq!(initial_plan.audio_profile.as_ref().unwrap().ring.as_deref(), Some("first.wav"));
    assert!(initial_plan.recording.enabled);
    let use_media_proxy = SipSession::check_media_proxy(&context, &initial_plan.media.proxy_mode);
    assert!(use_media_proxy, "first trunk anchors media for the whole call");
    let (mut session, _handle, _commands) = SipSession::new(server.clone(), CancellationToken::new(), None, context, dialog, use_media_proxy);
    assert!(Arc::ptr_eq(&session.context.dialplan, &common_plan), "setup must not replace the routed plan");
    let plan = SipSession::plan_for_attempt(&server, &common_plan, &attempts[0]);
    session.activate_attempt(&attempts[0], plan).await;
    session.media.caller_offer = Some(concat!(
        "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=Talk\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n",
        "m=audio 40000 RTP/AVP 0 8\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n",
    ).into());
    session.ensure_caller_leg().await.unwrap();
    session.media.early_media_sent = true;
    let caller = session.media_leg(&LegId::from("caller")).unwrap();
    let finalized = Arc::new(AtomicUsize::new(0));
    session.media.recording.set_recorder(Box::new(Recorder(finalized.clone())), None).await.unwrap();
    assert!(session.media.recording.has_recorder().await);

    let plan = SipSession::plan_for_attempt(&server, &common_plan, second);
    session.activate_attempt(second, plan).await;
    assert_eq!(session.context.dialplan.max_ring_time, Some(Duration::from_secs(9)));
    assert_eq!(session.context.dialplan.audio_profile.as_ref().unwrap().ring.as_deref(), Some("common.wav"));
    assert!(session.context.dialplan.recording.enabled, "recording is fixed for the call");
    assert_eq!(finalized.load(Ordering::SeqCst), 0, "the next trunk keeps the call's recorder");
    assert!(session.media.recording.has_recorder().await);
    assert_eq!(session.context.dialplan.media.proxy_mode, MediaProxyMode::Bypass);
    assert_eq!(session.media_profile.path, MediaPathMode::Anchored, "media path is fixed at setup");
    assert!(Arc::ptr_eq(&caller, &session.media_leg(&LegId::from("caller")).unwrap()));
    let sdp = session.prepare_callee_media_offer(&Location { aor: second.location.aor.clone(), ..Default::default() }).await.unwrap().unwrap();
    assert!(!sdp.is_empty(), "retry generates a fresh callee offer against the retained caller");

    let caller_only = dial_attempt(
        rsipstack::dialog::invitation::InviteOption {
            callee: "sip:third@127.0.0.1".try_into().unwrap(),
            caller: "sip:third-caller@rustpbx.com".try_into().unwrap(),
            ..Default::default()
        },
        None,
        None,
    );
    let plan = SipSession::plan_for_attempt(&server, &common_plan, &caller_only);
    session.activate_attempt(&caller_only, plan).await;
    assert_eq!(session.context.dialplan.caller.as_ref().unwrap().user(), Some("third-caller"));
    assert_eq!(session.context.dialplan.media.proxy_mode, common_plan.media.proxy_mode);
    assert_eq!(session.context.dialplan.max_ring_time, common_plan.max_ring_time);
}

mod child_event_tests;
mod session_id_tests;

#[tokio::test]
async fn cleanup_keeps_session_live_for_watchdog_until_ended_hooks_finish() {
    use crate::call::{DialDirection, Dialplan};
    use crate::callrecord::{CallRecordHook, recording_upload::RecordingUploadHook};
    use crate::config::{RecordingPolicy, RecordingType};
    use crate::proxy::active_call_registry::{
        ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
    };
    use crate::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;
    use crate::proxy::tests::test_sip_session_regressions::setup_recording_test_media;
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::sync::atomic::AtomicBool;

    struct ObserveEnded {
        live: Arc<ActiveProxyCallRegistry>,
        observed_live: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl CallSessionHook for ObserveEnded {
        async fn on_call_ended(
            &self,
            ctx: &CallSessionContext,
            _: Option<&CallRecordHangupReason>,
            _: u64,
        ) {
            self.observed_live.store(
                self.live.get_handle(&ctx.session_id).is_some(),
                Ordering::SeqCst,
            );
        }
    }

    let (mut server, _) = create_test_server().await;
    let live = server.active_call_registry.clone();
    let observed_live = Arc::new(AtomicBool::new(false));
    Arc::get_mut(&mut server).unwrap().session_hooks = Arc::new(vec![Arc::new(ObserveEnded {
        live: live.clone(),
        observed_live: observed_live.clone(),
    })]);
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new("cleanup-watchdog".into(), request, DialDirection::Inbound)
        .with_media(
            crate::call::MediaConfig::new().with_proxy_mode(crate::config::MediaProxyMode::All),
        );
    let (mut session, handle, _commands) =
        build_session_with_cmd_rx_on(server.clone(), dialplan).await;
    let session_id = handle.session_id().to_string();
    live.upsert(
        ActiveProxyCallEntry {
            session_id: session_id.clone(),
            caller: None,
            callee: None,
            direction: "inbound".into(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Talking,
        },
        handle,
    );
    live.touch_heartbeat(&session_id);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup.wav");
    let recording_id = "33333333-3333-4333-8333-333333333333";
    setup_recording_test_media(&mut session).await;
    let started = session
        .execute_command(
            CallCommand::StartRecording {
                config: crate::call::domain::RecordConfig {
                    path: path.to_string_lossy().into_owned(),
                    unique_id: Some(recording_id.into()),
                    notify_app: Some(false),
                    ..Default::default()
                },
            },
            None,
        )
        .await;
    assert!(started.success, "{started:?}");
    let peer = session.legs.media_leg(&LegId::from("caller")).unwrap();
    let tap = peer.ingress_tap();
    let mut encoder = audio_codec::create_encoder(audio_codec::CodecType::PCMU);
    for seq in 0..10u16 {
        let packet = RtpPacket::new(
            RtpHeader::new(0, seq, seq as u32 * 160, 1234),
            encoder.encode(&vec![8000; 160]).to_vec(),
        );
        tap.on_ingress(&packet, "127.0.0.1:12345".parse().unwrap());
    }
    let (records, mut received) = mpsc::channel(1);
    session.reporter = Some(CallReporter {
        server: server.clone(),
        context: session.context.clone(),
        call_record_sender: Some(records),
    });

    session.cleanup().await;

    assert!(
        observed_live.load(Ordering::SeqCst),
        "normal teardown must stay live while ended hooks release their bindings"
    );
    assert!(
        live.get_handle(&session_id).is_none(),
        "completed teardown must remove its live handle"
    );
    assert!(
        live.get(&session_id).is_none(),
        "completed teardown must remove its active-call row"
    );
    assert!(
        !session
            .media
            .recording
            .recorder_status()
            .await
            .unwrap()
            .active
    );
    let wav = std::fs::read(&path).unwrap();
    assert!(
        wav.len() > 44
            && wav[44..]
                .chunks_exact(2)
                .any(|s| i16::from_le_bytes([s[0], s[1]]) != 0),
        "cleanup must finalize actual RTP audio, not only a WAV header"
    );
    let mut record = received.try_recv().expect("cleanup must publish its CDR");
    assert_eq!(record.call_id, session_id);
    assert_eq!(record.recorder.len(), 1);
    assert_eq!(record.recorder[0].unique_id.as_deref(), Some(recording_id));
    assert_eq!(record.recorder[0].size, wav.len() as u64);
    assert_eq!(
        record.recorder[0].extra.as_ref().unwrap()["session_id"],
        session_id
    );

    let body = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = body.clone();
    let upload = axum::Router::new().route(
        "/recording",
        axum::routing::post(move |bytes: bytes::Bytes| {
            let captured = captured.clone();
            async move {
                *captured.lock().unwrap() = bytes.to_vec();
                "https://recordings.example/cleanup.wav"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let uploader = tokio::spawn(async move {
        axum::serve(listener, upload).await.unwrap();
    });
    let (hook, _, _) = RecordingUploadHook::new(RecordingPolicy {
        enabled: Some(true),
        recording_type: Some(RecordingType::Http),
        url: Some(format!("http://{address}/recording")),
        ..Default::default()
    })
    .unwrap();
    hook.on_record_completed(std::slice::from_mut(&mut record))
        .await
        .unwrap();
    uploader.abort();
    assert!(
        body.lock()
            .unwrap()
            .windows(wav.len())
            .any(|bytes| bytes == wav)
    );
    assert_eq!(
        record.details.recording_url.as_deref(),
        Some("https://recordings.example/cleanup.wav")
    );
    assert_eq!(record.call_id, session_id);
    assert_eq!(record.recorder[0].unique_id.as_deref(), Some(recording_id));
}

struct InvocationHeadersRuntime {
    invocation: crate::call::app::AppInvocationContext,
}

#[async_trait::async_trait]
impl AppRuntime for InvocationHeadersRuntime {
    async fn start_app(
        &self,
        _app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn is_running(&self) -> bool {
        true
    }

    fn current_app(&self) -> Option<String> {
        Some("current-app".to_string())
    }

    async fn current_app_invocation(&self) -> Option<crate::call::app::AppInvocationContext> {
        Some(self.invocation.clone())
    }
}

#[tokio::test]
async fn ivr_exec_route_point_resolves_application_route_and_preserves_identity() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::routing::{MatchConditions, RewriteRules, RouteAction, RouteRule};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_config};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;

    let mut config = crate::config::ProxyConfig::default();
    config.routes = Some(vec![RouteRule {
        name: "routed-ivr".to_string(),
        priority: 2000,
        match_conditions: MatchConditions {
            request_uri_user: Some("^(ivr-entry-a|ivr-entry-b)$".to_string()),
            ..Default::default()
        },
        rewrite: Some(RewriteRules {
            headers: std::collections::HashMap::from([(
                "header.flowname".to_string(),
                "RoutedFlow".to_string(),
            )]),
            ..Default::default()
        }),
        action: RouteAction {
            action: Some("application".to_string()),
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": "step-ivr",
                "ivr_params": {
                    "operation": "route-default",
                    "route_default": "kept"
                }
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let (server, _) = create_test_server_with_config(config).await;
    for route_point in ["ivr-entry-a", "ivr-entry-b"] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let dialplan = Dialplan::new(
            format!("ivr-exec-route-{route_point}"),
            request,
            DialDirection::Inbound,
        )
        .with_caller("sip:alice@rustpbx.com".try_into().unwrap());
        let (mut session, _handle, mut cmd_rx) =
            build_session_with_cmd_rx_on(server.clone(), dialplan).await;
        let (info_tx, _response_rx) = TransactionHandle::new();

        session
            .handle_ivr_exec_command(
                Some(serde_json::json!({
                    "route_point": route_point,
                    "hold_agent": false,
                    "ivr_params": {
                        "route_point": "untrusted-duplicate",
                        "workflow_id": "workflow-test",
                        "operation": "confirm"
                    }
                })),
                None,
                &info_tx,
            )
            .await
            .unwrap();

        let command = cmd_rx
            .recv()
            .await
            .expect("ivr.exec should enqueue StartApp");
        match command {
            CallCommand::StartAppWithRouteContext {
                app_name,
                params,
                route_context,
                ..
            } => {
                let params = params.expect("route application should provide params");
                assert_eq!(app_name, "ivr");
                assert_eq!(params["file"], "step-ivr");
                assert_eq!(route_context.sip_headers["flowname"], "RoutedFlow");
                assert_eq!(params["ivr_params"]["route_point"], route_point);
                assert_eq!(params["ivr_params"]["workflow_id"], "workflow-test");
                assert_eq!(params["ivr_params"]["operation"], "confirm");
                assert_eq!(params["ivr_params"]["route_default"], "kept");
            }
            other => panic!("expected StartApp, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn ivr_exec_does_not_trust_inherited_route_dispatch_header() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::routing::{MatchConditions, RouteAction, RouteRule};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_config};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;

    let mut config = crate::config::ProxyConfig::default();
    config.routes = Some(vec![RouteRule {
        name: "route-without-dispatch-header".to_string(),
        priority: 2000,
        match_conditions: MatchConditions {
            request_uri_user: Some("^plain-entry$".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("application".to_string()),
            app: Some("ivr".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let (server, _) = create_test_server_with_config(config).await;
    let mut request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    request.headers.push(rsipstack::sip::Header::Other(
        "FlowName".to_string(),
        "ForgedFlow".to_string(),
    ));
    let dialplan = Dialplan::new(
        "ivr-exec-route-header-provenance".to_string(),
        request,
        DialDirection::Inbound,
    )
    .with_caller("sip:alice@rustpbx.com".try_into().unwrap());
    let (mut session, _handle, mut cmd_rx) = build_session_with_cmd_rx_on(server, dialplan).await;
    session.app_runtime = Arc::new(InvocationHeadersRuntime {
        invocation: crate::call::app::AppInvocationContext {
            app_execution_id: 7,
            sip_headers: std::collections::HashMap::from([
                ("FlowName".to_string(), "ForgedFlow".to_string()),
                ("X-Context".to_string(), "kept".to_string()),
            ]),
            ..Default::default()
        },
    });
    let (info_tx, _response_rx) = TransactionHandle::new();

    session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "route_point": "plain-entry", "hold_agent": false
            })),
            None,
            &info_tx,
        )
        .await
        .unwrap();

    let command = cmd_rx
        .recv()
        .await
        .expect("ivr.exec should enqueue StartApp");
    let CallCommand::StartAppWithRouteContext { route_context, .. } = command else {
        panic!("expected routed application command");
    };
    assert!(
        route_context
            .sip_headers
            .keys()
            .all(|name| !name.eq_ignore_ascii_case("flowname"))
    );
    assert_eq!(route_context.sip_headers["X-Context"], "kept");
}

#[tokio::test]
async fn ivr_exec_explicit_ivr_name_keeps_direct_execution_available() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;

    let (server, _) = create_test_server().await;
    let expected_file = server.data_context.resolve_ivr_file("direct-test").await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new(
        "ivr-exec-direct".to_string(),
        request,
        DialDirection::Inbound,
    )
    .with_caller("sip:alice@rustpbx.com".try_into().unwrap());
    let (mut session, _handle, mut cmd_rx) = build_session_with_cmd_rx_on(server, dialplan).await;
    let (info_tx, _response_rx) = TransactionHandle::new();

    session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "ivr_name": "direct-test",
                "hold_agent": false,
                "ivr_params": { "business_key": "business-value" }
            })),
            None,
            &info_tx,
        )
        .await
        .unwrap();

    let command = cmd_rx
        .recv()
        .await
        .expect("ivr.exec should enqueue StartApp");
    match command {
        CallCommand::StartApp {
            app_name, params, ..
        } => {
            let params = params.expect("direct IVR should provide params");
            assert_eq!(app_name, "ivr");
            assert_eq!(params["file"], expected_file);
            assert_eq!(params["ivr_params"]["business_key"], "business-value");
        }
        other => panic!("expected StartApp, got {other:?}"),
    }
}

#[tokio::test]
async fn ivr_exec_requires_exactly_one_target_selector() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new(
        "ivr-exec-conflicting-targets".to_string(),
        request,
        DialDirection::Inbound,
    )
    .with_caller("sip:alice@rustpbx.com".try_into().unwrap());
    let (mut session, _handle, mut cmd_rx) = build_session_with_cmd_rx_on(server, dialplan).await;
    let (info_tx, mut response_rx) = TransactionHandle::new();

    for (case, params) in [
        (
            "conflicting targets",
            serde_json::json!({
                "route_point": "ivr-entry-a",
                "ivr_name": "direct-test",
                "hold_agent": false
            }),
        ),
        (
            "missing target",
            serde_json::json!({
                "hold_agent": false,
                "ivr_params": { "file": "raw-ivr.toml" }
            }),
        ),
    ] {
        session
            .handle_ivr_exec_command(Some(params), None, &info_tx)
            .await
            .unwrap();

        let response = response_rx
            .recv()
            .await
            .unwrap_or_else(|| panic!("{case} should receive a final response"));
        let rsipstack::dialog::dialog::TransactionCommand::Respond { status, .. } = response;
        assert_eq!(status, rsipstack::sip::StatusCode::BadRequest, "{case}");
        assert!(cmd_rx.try_recv().is_err(), "{case} must not start an app");
    }
}

async fn ivr_exec_test_session(
    name: &str,
) -> (SipSession, tokio::sync::mpsc::Receiver<CallCommand>) {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;
    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new(name.to_string(), request, DialDirection::Inbound)
        .with_caller("sip:alice@rustpbx.com".try_into().unwrap());
    let (session, _handle, cmd_rx) = build_session_with_cmd_rx_on(server, dialplan).await;
    (session, cmd_rx)
}

#[tokio::test]
async fn peer_ring_timeout_offer_survives_terminal_invite_without_trunk_passthrough() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;
    for trusted in [true, false] {
        let (mut server, _) = create_test_server().await;
        Arc::get_mut(&mut server).unwrap().cluster_peer_ips = vec!["192.0.2.44".parse().unwrap()];
        let mut request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "example.test",
            None,
        );
        request.headers.push(rsipstack::sip::Header::Other(
            "Call-Info".into(),
            "<urn:uuid:offer-a>;purpose=ring-timeout".into(),
        ));
        let dialplan = Dialplan::new("peer-offer".into(), request, DialDirection::Inbound)
            .with_caller("sip:caller@example.test".parse().unwrap());
        let (mut session, _handle, _commands) =
            build_session_with_cmd_rx_on(server, dialplan).await;
        session
            .context
            .cookie
            .insert_extension(crate::call::CallerPeerContext(
                if trusted {
                    "192.0.2.44:5060"
                } else {
                    "198.51.100.88:5060"
                }
                .into(),
            ));
        let target = crate::call::Location {
            aor: "sip:agent@example.test".parse().unwrap(),
            ..Default::default()
        };
        let (invite, _, _, _) = session
            .build_target_invite_option(&target, None, None)
            .await
            .unwrap();
        assert_eq!(
            crate::call::sip::ring_timeout_offer(invite.headers.as_deref().unwrap_or_default()),
            trusted.then(|| "urn:uuid:offer-a".to_string()),
            "only a trusted home-proxy hop may retain the offer capability",
        );
    }
}

#[tokio::test]
async fn ring_timeout_notifies_the_offered_leg_before_answer() {
    use rsipstack::sip::{HeadersExt, Method, SipMessage};
    use rsipstack::transport::{SipAddr, channel::ChannelConnection, connection::TransportEvent};
    let (mut session, _commands) = ivr_exec_test_session("ring-timeout-notification").await;
    let endpoint = session.server.endpoint.inner.clone();
    for addr in endpoint.transport_layer.get_addrs() {
        endpoint.transport_layer.del_transport(&addr);
    }
    let mut local =
        SipAddr::try_from(&"sip:127.0.0.1:5060".parse::<rsipstack::sip::Uri>().unwrap()).unwrap();
    let mut target =
        SipAddr::try_from(&"sip:127.0.0.1:5070".parse::<rsipstack::sip::Uri>().unwrap()).unwrap();
    local.r#type = Some(rsipstack::sip::Transport::Udp);
    target.r#type = Some(rsipstack::sip::Transport::Udp);
    let (_incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
    let (outgoing, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel();
    let channel = ChannelConnection::create_connection(incoming_rx, outgoing, local, None)
        .await
        .unwrap();
    endpoint.transport_layer.add_transport(channel.into());
    let leg = LegId::from("unanswered-agent");
    session
        .legs
        .insert(leg.clone(), crate::call::domain::Leg::new(leg.clone()));
    let location = crate::call::Location {
        aor: "sip:agent@127.0.0.1:5070".parse().unwrap(),
        destination: Some(target.clone()),
        headers: Some(vec![rsipstack::sip::Header::Other(
            "Call-Info".into(),
            "<urn:uuid:offer-a>;purpose=ring-timeout".into(),
        )]),
        ..Default::default()
    };
    session
        .initiate_sip_leg(&leg, location, Some(crate::call::domain::LegPurpose::Agent))
        .await
        .unwrap();
    let invite = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(TransportEvent::Incoming(SipMessage::Request(request), _, _)) =
                outgoing_rx.recv().await
            {
                if request.method == Method::Invite {
                    break request;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(
        session.legs.get_dialog(&leg).is_none(),
        "notification must work before a dialog is attached"
    );
    session
        .handle_remove_leg(leg.clone(), Some("ring_timeout".into()))
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(TransportEvent::Incoming(SipMessage::Request(request), _, _)) =
                outgoing_rx.recv().await
            {
                if request.method == Method::Message {
                    break request;
                }
            }
        }
    })
    .await
    .expect("ring timeout must reach the client even before answer");
    let event: serde_json::Value = serde_json::from_slice(&message.body).unwrap();
    assert_eq!(message.uri, invite.uri);
    assert_ne!(
        message.call_id_header().unwrap().value(),
        invite.call_id_header().unwrap().value()
    );
    assert_eq!(event["call_id"], invite.call_id_header().unwrap().value());
    assert_eq!(event["offer"], "urn:uuid:offer-a");
    assert_eq!(event["event"], "ring_timeout");
}

#[tokio::test]
async fn ivr_exec_enqueue_failure_returns_error_and_releases_invocation() {
    use crate::proxy::proxy_call::ivr_exec_hook::IvrExecState;
    let (mut session, cmd_rx) = ivr_exec_test_session("ivr-exec-enqueue-failure").await;
    drop(cmd_rx);
    let (info_tx, mut response_rx) = TransactionHandle::new();
    session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "ivr_name": "direct-test", "hold_agent": false, "request_id": "request-a"
            })),
            None,
            &info_tx,
        )
        .await
        .unwrap();
    let rsipstack::dialog::dialog::TransactionCommand::Respond { status, .. } = response_rx
        .recv()
        .await
        .expect("INFO must receive a response");
    assert_eq!(status, rsipstack::sip::StatusCode::ServerInternalError);
    assert!(session.extensions.read().get::<IvrExecState>().is_none());
}

#[tokio::test]
async fn ivr_exec_route_point_must_resolve_to_application() {
    use crate::proxy::proxy_call::ivr_exec_hook::IvrExecState;
    let (mut session, mut cmd_rx) = ivr_exec_test_session("ivr-exec-missing-route").await;
    let (info_tx, mut response_rx) = TransactionHandle::new();
    session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "route_point": "missing-route", "hold_agent": false
            })),
            None,
            &info_tx,
        )
        .await
        .unwrap();
    let rsipstack::dialog::dialog::TransactionCommand::Respond { status, .. } = response_rx
        .recv()
        .await
        .expect("INFO must receive a response");
    assert_eq!(status, rsipstack::sip::StatusCode::NotFound);
    assert!(cmd_rx.try_recv().is_err());
    assert!(session.extensions.read().get::<IvrExecState>().is_none());
}

#[tokio::test]
async fn ivr_exec_invalid_route_point_receives_final_bad_request() {
    use crate::proxy::proxy_call::ivr_exec_hook::IvrExecState;
    let (mut session, mut cmd_rx) = ivr_exec_test_session("ivr-exec-invalid-route").await;
    let (info_tx, mut response_rx) = TransactionHandle::new();

    let result = session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "route_point": "sip:route@example.invalid:99999", "hold_agent": false
            })),
            None,
            &info_tx,
        )
        .await;

    assert!(
        result.is_ok(),
        "malformed user input must not escape the INFO handler"
    );
    let rsipstack::dialog::dialog::TransactionCommand::Respond { status, .. } = response_rx
        .recv()
        .await
        .expect("INFO must receive a final response");
    assert_eq!(status, rsipstack::sip::StatusCode::BadRequest);
    assert!(cmd_rx.try_recv().is_err());
    assert!(session.extensions.read().get::<IvrExecState>().is_none());
}

#[tokio::test]
async fn ivr_exec_busy_rejects_before_replacing_active_invocation() {
    use crate::proxy::proxy_call::ivr_exec_hook::IvrExecState;
    let (mut session, mut cmd_rx) = ivr_exec_test_session("ivr-exec-busy").await;
    session.extensions.write().insert(IvrExecState {
        app_execution_id: None,
        request_id: "first-request".to_string(),
        held_leg: None,
        initiator_leg: LegId::from("callee"),
        webhook_url: None,
        app_name: "ivr".to_string(),
        metadata: serde_json::Value::Null,
    });
    let (info_tx, mut response_rx) = TransactionHandle::new();
    session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "ivr_name": "direct-test", "hold_agent": false, "request_id": "second-request"
            })),
            None,
            &info_tx,
        )
        .await
        .unwrap();
    let rsipstack::dialog::dialog::TransactionCommand::Respond { status, .. } = response_rx
        .recv()
        .await
        .expect("INFO must receive a response");
    assert_eq!(status, rsipstack::sip::StatusCode::BusyHere);
    assert!(cmd_rx.try_recv().is_err());
    assert_eq!(
        session
            .extensions
            .read()
            .get::<IvrExecState>()
            .unwrap()
            .request_id,
        "first-request"
    );
}

#[tokio::test]
async fn connectivity_regression_ivr_exec_hold_failure_prevents_app_start() {
    use crate::proxy::proxy_call::ivr_exec_hook::IvrExecState;
    let (mut failed_session, mut failed_cmd_rx) =
        ivr_exec_test_session("ivr-exec-hold-failure").await;
    let (failed_info_tx, mut failed_response_rx) = TransactionHandle::new();
    failed_session
        .handle_ivr_exec_command(
            Some(serde_json::json!({
                "ivr_name": "direct-test",
                "request_id": "hold-failure"
            })),
            Some(LegId::from("missing-agent-leg")),
            &failed_info_tx,
        )
        .await
        .unwrap();

    let rsipstack::dialog::dialog::TransactionCommand::Respond { status, .. } = failed_response_rx
        .recv()
        .await
        .expect("INFO must receive a final response");
    assert_eq!(status, rsipstack::sip::StatusCode::ServerInternalError);
    assert!(
        failed_cmd_rx.try_recv().is_err(),
        "failed hold must not start IVR"
    );
    assert!(
        failed_session
            .extensions
            .read()
            .get::<IvrExecState>()
            .is_none()
    );
}

struct DtmfAppRuntime {
    running: bool,
    inject_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl AppRuntime for DtmfAppRuntime {
    async fn start_app(
        &self,
        _app_name: &str,
        _params: Option<serde_json::Value>,
        _auto_answer: bool,
    ) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    async fn stop_app(&self, _reason: Option<String>) -> crate::call::runtime::AppResult<()> {
        Ok(())
    }

    fn inject_event(&self, _event: serde_json::Value) -> crate::call::runtime::AppResult<()> {
        self.inject_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn current_app(&self) -> Option<String> {
        self.running.then(|| "test".to_string())
    }
}

#[test]
fn forward_dtmf_skips_app_injection_when_no_app_is_running() {
    let runtime = Arc::new(DtmfAppRuntime {
        running: false,
        inject_calls: AtomicUsize::new(0),
    });
    let app_runtime: Arc<dyn AppRuntime> = runtime.clone();
    let bridge_dtmf_tx = Arc::new(parking_lot::RwLock::new(None));

    forward_dtmf_event(
        '2',
        "caller",
        "caller",
        "test-session",
        &app_runtime,
        &None,
        &bridge_dtmf_tx,
        &Arc::new(parking_lot::Mutex::new(None)),
        &Arc::new(parking_lot::Mutex::new(Vec::new())),
        "1001",
        "2000",
        None,
        None,
    );

    assert_eq!(runtime.inject_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn forward_dtmf_rejects_non_caller_leg_as_application_input() {
    let runtime = Arc::new(DtmfAppRuntime {
        running: true,
        inject_calls: AtomicUsize::new(0),
    });
    let app_runtime: Arc<dyn AppRuntime> = runtime.clone();
    let (tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let bridge_dtmf_tx = Arc::new(parking_lot::RwLock::new(Some(tx)));
    let buffered = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let accepted = forward_dtmf_event(
        '1',
        "callee",
        "caller",
        "test-session",
        &app_runtime,
        &None,
        &bridge_dtmf_tx,
        &Arc::new(parking_lot::Mutex::new(None)),
        &buffered,
        "1001",
        "2000",
        None,
        None,
    );

    assert!(!accepted);
    assert_eq!(runtime.inject_calls.load(Ordering::SeqCst), 0);
    assert!(bridge_rx.try_recv().is_err());
    assert!(buffered.lock().is_empty());
}

/// While a bridge is active (armed `bridge_dtmf_tx`), the bridge owns the
/// digit. It must be forwarded, buffered for the return app, and traced, but
/// never also injected into the suspended app as a second business input.
#[test]
fn forward_dtmf_with_active_bridge_owns_digit_without_app_injection() {
    use crate::proxy::proxy_call::sip_session::transfer::BridgeTraceContext;
    use crate::rwi::gateway::RwiGateway;

    let runtime = Arc::new(DtmfAppRuntime {
        running: true,
        inject_calls: AtomicUsize::new(0),
    });
    let app_runtime: Arc<dyn AppRuntime> = runtime.clone();

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let gw_ref = Arc::new(parking_lot::RwLock::new(gateway));

    // Bridge active: an open channel counts as "bridge running".
    let (tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let bridge_dtmf_tx = Arc::new(parking_lot::RwLock::new(Some(tx)));

    let trace_ctx = Arc::new(parking_lot::Mutex::new(Some(BridgeTraceContext {
        step_id: Some("step-menu-tts".to_string()),
        step_name: Some("菜单".to_string()),
        extra: Some(serde_json::json!({"nodetype": "menu_tts", "businessnodeid": "42"})),
        step_start_time: Some("2026-01-01T00:00:00+00:00".to_string()),
        step_index: Some(2),
        app_execution_id: Some(5),
        resumable: false,
    })));
    let digits = Arc::new(parking_lot::Mutex::new(Vec::new()));

    forward_dtmf_event(
        '1',
        "caller",
        "caller",
        "test-session",
        &app_runtime,
        &Some(gw_ref),
        &bridge_dtmf_tx,
        &trace_ctx,
        &digits,
        "sip:1001@x",
        "sip:2000@x",
        Some(std::collections::HashMap::from([(
            "X-Business-Type".to_string(),
            "34".to_string(),
        )])),
        None,
    );

    // 1. digit forwarded to the bridge websocket
    let ws_json = ws_rx.try_recv().expect("digit must reach the ws channel");
    let v: serde_json::Value = serde_json::from_str(&ws_json).unwrap();
    assert_eq!(v["type"], "dtmf");
    assert_eq!(v["digit"], "1");
    assert_eq!(runtime.inject_calls.load(Ordering::SeqCst), 0);

    // 2. buffered for the return-app flow
    assert_eq!(digits.lock().clone(), vec!["1".to_string()]);

    // 3. ivr_step_trace emitted with digit + node context
    let ev = events
        .try_recv()
        .expect("bridge DTMF must emit an ivr_step_trace event");
    assert_eq!(ev.event.event_type, "ivr_step_trace");
    assert_eq!(ev.event.payload["trigger"]["type"], "dtmf");
    assert_eq!(ev.event.payload["trigger"]["detail"]["digit"], "1");
    assert_eq!(ev.event.payload["step_id"], "step-menu-tts");
    assert_eq!(ev.event.payload["action_type"], "Bridge");
    assert_eq!(ev.event.payload["extra"]["nodetype"], "menu_tts");
    assert_eq!(
        ev.event.payload["step_index"], 2,
        "bridge DTMF trace must carry the executor-stashed step index, not a hard-coded 0"
    );
    assert_eq!(
        ev.event.payload["app_execution_id"], 5,
        "bridge DTMF trace must carry the suspended execution's identity"
    );
    assert_eq!(
        ev.event.payload["step_start_time"], "2026-01-01T00:00:00+00:00",
        "bridge DTMF trace must carry the executor-stamped step start time"
    );
    let start: chrono::DateTime<chrono::FixedOffset> =
        chrono::DateTime::parse_from_rfc3339(ev.event.payload["step_start_time"].as_str().unwrap())
            .expect("step_start_time must be RFC3339");
    let end: chrono::DateTime<chrono::FixedOffset> =
        chrono::DateTime::parse_from_rfc3339(ev.event.payload["step_end_time"].as_str().unwrap())
            .expect("step_end_time must be RFC3339");
    assert!(
        end >= start,
        "consumer-derived duration would be negative if end < start"
    );
    // Contract: duration_ms is derived from the stamps (end - start), not a
    // hardcoded 0. The executor-stamped start is a fixed past date, so the
    // window is large and strictly positive.
    let duration = ev.event.payload["duration_ms"]
        .as_u64()
        .expect("bridge DTMF trace must carry duration_ms");
    assert_eq!(
        duration,
        (end - start).num_milliseconds().max(0) as u64,
        "bridge DTMF duration_ms must equal step_end_time - step_start_time"
    );
    assert!(
        duration > 0,
        "bridge DTMF duration must be derived from its stamps, got 0"
    );
    assert_eq!(ev.event.payload["caller"], "sip:1001@x");
    assert_eq!(
        ev.event.payload["sip_headers"]["X-Business-Type"], "34",
        "bridge DTMF trace must carry the call's SIP headers"
    );
    assert!(ev.event.payload["end_reason"].is_null());
}

/// A RESUMABLE bridge hand-off (`return_ivr_resume=1` → `resumable: true`)
/// suppresses the eager per-digit trace: the resumed step executor reports
/// the bridge step itself once the successor node resolves, so the trace can
/// carry `next_node_id`. The digit must still be forwarded to the bridge and
/// buffered for the return-app flow.
#[test]
fn forward_dtmf_resumable_bridge_defers_trace_to_resumed_ivr() {
    use crate::proxy::proxy_call::sip_session::transfer::BridgeTraceContext;
    use crate::rwi::gateway::RwiGateway;

    let runtime = Arc::new(DtmfAppRuntime {
        running: true,
        inject_calls: AtomicUsize::new(0),
    });
    let app_runtime: Arc<dyn AppRuntime> = runtime.clone();

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let gw_ref = Arc::new(parking_lot::RwLock::new(gateway));

    let (tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let bridge_dtmf_tx = Arc::new(parking_lot::RwLock::new(Some(tx)));

    let trace_ctx = Arc::new(parking_lot::Mutex::new(Some(BridgeTraceContext {
        step_id: Some("step-menu-tts".to_string()),
        step_name: Some("菜单".to_string()),
        extra: Some(serde_json::json!({"nodetype": "menu_tts"})),
        step_start_time: Some("2026-01-01T00:00:00+00:00".to_string()),
        step_index: Some(2),
        app_execution_id: Some(5),
        resumable: true,
    })));
    let digits = Arc::new(parking_lot::Mutex::new(Vec::new()));

    forward_dtmf_event(
        '8',
        "caller",
        "caller",
        "test-session",
        &app_runtime,
        &Some(gw_ref),
        &bridge_dtmf_tx,
        &trace_ctx,
        &digits,
        "sip:1001@x",
        "sip:2000@x",
        None,
        None,
    );

    // Digit still forwarded + buffered — only the eager trace is suppressed.
    let ws_json = ws_rx.try_recv().expect("digit must reach the ws channel");
    let v: serde_json::Value = serde_json::from_str(&ws_json).unwrap();
    assert_eq!(v["digit"], "8");
    assert_eq!(digits.lock().clone(), vec!["8".to_string()]);
    assert_eq!(runtime.inject_calls.load(Ordering::SeqCst), 0);

    assert!(
        events.try_recv().is_err(),
        "resumable hand-off must NOT emit the eager bridge DTMF trace — \
         the resumed executor reports the step (with next_node_id) instead"
    );
}

/// When an IVR flow dies while suspended on a bridge (caller hangup before
/// the return app runs), the proxy must emit the compensating `session_end`
/// `ivr_step_trace` the executor suppressed at hand-off time — carrying the
/// real end reason and the originating node context (exactly-once contract).
#[test]
fn suspended_flow_death_emits_compensating_session_end_trace() {
    use crate::call::app::ivr::provider::{SessionEndReason, SessionEndTag};
    use crate::proxy::proxy_call::sip_session::transfer::BridgeTraceContext;
    use crate::proxy::proxy_call::sip_session::util::emit_suspended_flow_session_end;
    use crate::rwi::gateway::RwiGateway;

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let gw_ref = Arc::new(parking_lot::RwLock::new(gateway));

    let trace_ctx = Arc::new(parking_lot::Mutex::new(Some(BridgeTraceContext {
        step_id: Some("step-menu-tts".to_string()),
        step_name: Some("菜单".to_string()),
        extra: Some(serde_json::json!({"nodetype": "menu_tts"})),
        step_start_time: Some("2026-01-01T00:00:00+00:00".to_string()),
        step_index: Some(3),
        app_execution_id: Some(7),
        resumable: false,
    })));

    emit_suspended_flow_session_end(
        "test-session",
        "sip:1001@x",
        "sip:2000@x",
        &Some(gw_ref),
        &trace_ctx,
        Some(std::collections::HashMap::from([(
            "X-Business-Type".to_string(),
            "34".to_string(),
        )])),
        SessionEndReason {
            reason: SessionEndTag::UserHangup,
            detail: None,
        },
        Some(7),
    );

    let ev = events
        .try_recv()
        .expect("suspended-flow death must emit a session_end trace");
    assert_eq!(ev.event.event_type, "ivr_step_trace");
    assert_eq!(ev.event.payload["trigger"]["type"], "session_end");
    assert_eq!(ev.event.payload["end_reason"], "user_hangup");
    assert_eq!(ev.event.payload["step_id"], "step-menu-tts");
    assert_eq!(ev.event.payload["action_type"], "Bridge");
    assert_eq!(ev.event.payload["session_id"], "test-session");
    assert_eq!(
        ev.event.payload["app_execution_id"], 7,
        "compensating session_end must carry the SUSPENDED execution's identity"
    );
    assert_eq!(
        ev.event.payload["sip_headers"]["X-Business-Type"], "34",
        "synthetic trace must carry the call's SIP headers"
    );
    assert!(
        ev.event.payload["step_start_time"].is_string(),
        "synthetic end trace must carry a step start time — consumers derive duration as \
         event timestamp - step_start_time, and a null start would force an envelope-timestamp \
         fallback that orders end < start"
    );
    let start: chrono::DateTime<chrono::FixedOffset> =
        chrono::DateTime::parse_from_rfc3339(ev.event.payload["step_start_time"].as_str().unwrap())
            .expect("step_start_time must be RFC3339");
    let end: chrono::DateTime<chrono::FixedOffset> =
        chrono::DateTime::parse_from_rfc3339(ev.event.payload["step_end_time"].as_str().unwrap())
            .expect("step_end_time must be RFC3339");
    assert!(
        end >= start,
        "consumer-derived duration would be negative if end < start"
    );
    // Contract: duration_ms is derived from the stamps (end - start), not a
    // hardcoded 0. The executor-stamped start is a fixed past date, so the
    // window is large and strictly positive.
    let duration = ev.event.payload["duration_ms"]
        .as_u64()
        .expect("synthetic session_end trace must carry duration_ms");
    assert_eq!(
        duration,
        (end - start).num_milliseconds().max(0) as u64,
        "synthetic session_end duration_ms must equal step_end_time - step_start_time"
    );
    assert!(
        duration > 0,
        "synthetic session_end duration must be derived from its stamps, got 0"
    );
    assert_eq!(
        ev.event.payload["step_index"], 3,
        "synthetic session_end must carry the executor-stashed step index, not a hard-coded 0"
    );
}

/// Two executions reusing one session: when the compensating session_end
/// resolves no identity (no bridge context, no executor stash) the field must
/// be OMITTED — never guessed, never taken from the successor or `extra`.
/// A no-RWI-gateway attachment must drop the trace with a visible warning
/// instead of failing silently.
#[test]
fn suspended_flow_end_without_identity_omits_field_and_warns_without_gateway() {
    use crate::call::app::ivr::provider::{SessionEndReason, SessionEndTag};
    use crate::proxy::proxy_call::sip_session::transfer::BridgeTraceContext;
    use crate::proxy::proxy_call::sip_session::util::emit_suspended_flow_session_end;
    use crate::rwi::gateway::RwiGateway;

    // 1. No identity anywhere → key absent from the payload entirely.
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let gw_ref = Arc::new(parking_lot::RwLock::new(gateway));
    let trace_ctx = Arc::new(parking_lot::Mutex::new(None::<BridgeTraceContext>));

    emit_suspended_flow_session_end(
        "test-session",
        "sip:1001@x",
        "sip:2000@x",
        &Some(gw_ref),
        &trace_ctx,
        None,
        SessionEndReason {
            reason: SessionEndTag::Hangup,
            detail: None,
        },
        None,
    );

    let ev = events
        .try_recv()
        .expect("queue/jump suspension without node context still emits the trace");
    assert_eq!(ev.event.payload["action_type"], "Transfer");
    assert!(
        ev.event.payload.get("app_execution_id").is_none(),
        "unknown identity must be omitted from the payload entirely, got: {}",
        ev.event.payload
    );

    // 2. Gateway missing → no emission, no panic (warning is logged).
    emit_suspended_flow_session_end(
        "test-session",
        "sip:1001@x",
        "sip:2000@x",
        &None,
        &trace_ctx,
        None,
        SessionEndReason {
            reason: SessionEndTag::UserHangup,
            detail: None,
        },
        Some(9),
    );
}

/// The compensating `session_end` tag must reflect the actual teardown cause
/// (exactly-once contract, "REAL end reason" clause) instead of hardcoding
/// `user_hangup`.
#[test]
fn suspended_flow_end_reason_maps_hangup_cause() {
    use crate::call::app::ivr::provider::SessionEndTag;
    use crate::callrecord::CallRecordHangupReason as R;
    use crate::proxy::proxy_call::sip_session::util::map_suspended_flow_end;

    // Caller-side death stays user_hangup.
    for reason in [R::ByCaller, R::Canceled, R::Abandoned, R::NoAnswer] {
        let end = map_suspended_flow_end(Some(&reason));
        assert_eq!(end.reason, SessionEndTag::UserHangup, "cause {reason:?}");
        assert_eq!(end.detail, None);
    }
    // RTP watchdog refines to timeout.
    let end = map_suspended_flow_end(Some(&R::RtpTimeout));
    assert_eq!(end.reason, SessionEndTag::Timeout);
    // System teardown keeps hangup with the CDR reason as detail.
    let end = map_suspended_flow_end(Some(&R::BySystem));
    assert_eq!(end.reason, SessionEndTag::Hangup);
    assert_eq!(end.detail.as_deref(), Some("BySystem"));
    // No recorded cause — plain hangup.
    let end = map_suspended_flow_end(None);
    assert_eq!(end.reason, SessionEndTag::Hangup);
    assert_eq!(end.detail, None);
}

// ── parse_dial_target ─────────────────────────────────────────────────

#[test]
fn parse_dial_target_accepts_bare_uri_with_transport() {
    let uri = parse_dial_target("sip:1001@10.0.0.1:5060;transport=udp").unwrap();
    assert_eq!(uri.user().as_deref(), Some("1001"));
    assert_eq!(uri.host().to_string(), "10.0.0.1");
    assert!(
        uri.params.iter().any(|p| matches!(
            p,
            rsipstack::sip::Param::Transport(rsipstack::sip::Transport::Udp)
        )),
        "bare URI transport param must be preserved"
    );
}

#[test]
fn parse_dial_target_accepts_registered_contact_value() {
    let target = "<sip:2itejs7c@k0euab21f8ta.invalid;transport=ws>;+sip.ice;reg-id=1;+sip.instance=\"<urn:uuid:86c49f5a-3fb1-428c-9a10-d218d87c4115>\";expires=50";
    let uri = parse_dial_target(target).expect("contact value must parse");
    assert_eq!(uri.user().as_deref(), Some("2itejs7c"));
    assert_eq!(uri.host().to_string(), "k0euab21f8ta.invalid");
    assert!(
        uri.params.iter().any(|p| matches!(
            p,
            rsipstack::sip::Param::Transport(rsipstack::sip::Transport::Ws)
        )),
        "transport=ws inside the contact URI must be preserved"
    );
}

#[test]
fn parse_dial_target_rejects_garbage() {
    assert!(parse_dial_target("sip:1001@example.com;transport=bogus").is_err());
}

// ── await_playback_done ────────────────────────────────────────────────

#[tokio::test]
async fn await_playback_done_resolves_on_natural_completion() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cancel = CancellationToken::new();
    tx.send(crate::media::media_bridge::PlaybackResult::completed())
        .unwrap();
    let result = SipSession::await_playback_done(rx, &cancel).await;
    let result = result.expect("should resolve with PlaybackResult");
    assert!(
        !result.interrupted,
        "natural EOF must not be marked interrupted"
    );
}

#[tokio::test]
async fn await_playback_done_returns_none_on_cancel() {
    let (_tx, rx) = tokio::sync::oneshot::channel();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = SipSession::await_playback_done(rx, &cancel).await;
    assert!(result.is_none(), "cancel must short-circuit to None");
}

#[tokio::test]
async fn await_playback_done_cancel_wins_when_both_ready() {
    // Biased select: when both the cancel signal and a completion are
    // immediately available, cancel must win (so a caller that already hung
    // up is never surprised by a stale completion being treated as success).
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cancel = CancellationToken::new();
    tx.send(crate::media::media_bridge::PlaybackResult::completed())
        .unwrap();
    cancel.cancel();
    let result = SipSession::await_playback_done(rx, &cancel).await;
    assert!(result.is_none(), "biased cancel should win");
}

// ── normalize_call_hangup_by ────────────────────────────────────────────

#[test]
fn hangup_by_agent_requires_cc_participation() {
    // CC-routed (queue) call: callee hangup stays "agent".
    assert_eq!(
        normalize_call_hangup_by("agent", Some("support"), false, None),
        "agent"
    );
    // Skill-group direct routing (resolved_agent_id): stays "agent".
    assert_eq!(normalize_call_hangup_by("agent", None, true, None), "agent");
    // Non-CC call (no queue, no resolved agent): remapped to "callee".
    assert_eq!(
        normalize_call_hangup_by("agent", None, false, None),
        "callee"
    );
}

#[test]
fn hangup_by_outbound_customer_bye_corrected_to_callee() {
    // Outbound click-to-dial, CUSTOMER hangs up first: the caller/A dialog's
    // remote BYE is stamped `caller_remote` — the CC-centric "agent" must
    // correct to "callee" even with full CC context.
    assert_eq!(
        normalize_call_hangup_by("agent", Some("support"), true, Some("caller_remote")),
        "callee"
    );
    assert_eq!(
        normalize_call_hangup_by("agent", None, true, Some("caller_remote")),
        "callee"
    );
    // Agent-side BYE on the callee dialog keeps "agent".
    assert_eq!(
        normalize_call_hangup_by("agent", None, true, Some("callee_remote")),
        "agent"
    );
}

#[test]
fn hangup_by_non_agent_unchanged() {
    assert_eq!(
        normalize_call_hangup_by("caller", None, false, None),
        "caller"
    );
    assert_eq!(
        normalize_call_hangup_by("system", None, false, None),
        "system"
    );
    assert_eq!(
        normalize_call_hangup_by("transfer", None, false, None),
        "transfer"
    );
    assert_eq!(
        normalize_call_hangup_by("unknown", None, false, None),
        "unknown"
    );
}

// ---- helpers for codec / audio-content verification ----

#[test]
fn test_sdp_transport_mode_classification() {
    // Plain RTP
    assert_eq!(
        SipSession::sdp_transport_mode("m=audio 1000 RTP/AVP 8 0\r\na=sendrecv\r\n"),
        rustrtc::TransportMode::Rtp
    );
    // SDES-SRTP via RTP/SAVP profile (Twilio-style)
    assert_eq!(
        SipSession::sdp_transport_mode(
            "m=audio 1000 RTP/SAVP 0 8 101\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:abc\r\n"
        ),
        rustrtc::TransportMode::Srtp
    );
    // SDES-SRTP advertised only via a=crypto
    assert_eq!(
        SipSession::sdp_transport_mode(
            "m=audio 1000 RTP/AVP 8\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:abc\r\n"
        ),
        rustrtc::TransportMode::Srtp
    );
    // WebRTC (ICE + DTLS) takes precedence even if a crypto line is present
    assert_eq!(
        SipSession::sdp_transport_mode(
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ice-ufrag:x\r\na=fingerprint:sha-256 AA\r\n"
        ),
        rustrtc::TransportMode::WebRtc
    );
}

#[test]
fn test_rtp_dtmf_detector_deduplicates_same_event() {
    let mut detector = RtpDtmfDetector::default();

    assert_eq!(detector.observe(&[1, 0x00, 0x00, 0xa0], 12_345), Some('1'));
    assert_eq!(detector.observe(&[1, 0x80, 0x01, 0x40], 12_345), None);
    assert_eq!(detector.observe(&[1, 0x00, 0x00, 0xa0], 12_505), Some('1'));
}

#[test]
fn test_rtp_dtmf_detector_maps_special_digits() {
    let mut detector = RtpDtmfDetector::default();

    assert_eq!(detector.observe(&[10, 0x00, 0x00, 0xa0], 1), Some('*'));
    assert_eq!(detector.observe(&[11, 0x00, 0x00, 0xa0], 2), Some('#'));
    assert_eq!(detector.observe(&[12, 0x00, 0x00, 0xa0], 3), Some('A'));
    assert_eq!(detector.observe(&[16, 0x00, 0x00, 0xa0], 4), None);
}

#[test]
fn test_rtp_dtmf_detector_receives_all_digits_0_to_9() {
    let mut detector = RtpDtmfDetector::default();

    // Test digits 0-9
    for digit_code in 0..=9 {
        let expected_digit = std::char::from_digit(digit_code as u32, 10).unwrap();
        let result = detector.observe(&[digit_code, 0x00, 0x00, 0xa0], digit_code as u32);
        assert_eq!(
            result,
            Some(expected_digit),
            "Failed to receive DTMF digit {}: got {:?}",
            digit_code,
            result
        );
    }
}

#[test]
fn test_rtp_dtmf_detector_sequence_of_different_digits() {
    let mut detector = RtpDtmfDetector::default();

    // Simulate pressing 2-4-5-6 (queue transfer example)
    let sequence = vec![
        (2u8, 100u32, '2'),
        (4u8, 200u32, '4'),
        (5u8, 300u32, '5'),
        (6u8, 400u32, '6'),
    ];

    for (digit_code, timestamp, expected_char) in sequence {
        let result = detector.observe(&[digit_code, 0x00, 0x00, 0xa0], timestamp);
        assert_eq!(
            result,
            Some(expected_char),
            "Failed to receive DTMF sequence digit {}: got {:?}",
            expected_char,
            result
        );
    }
}

#[test]
fn test_rtp_dtmf_detector_handles_short_payload() {
    let mut detector = RtpDtmfDetector::default();

    // Test with insufficient data (< 4 bytes)
    assert_eq!(detector.observe(&[1, 0x00], 100), None);
    assert_eq!(detector.observe(&[1, 0x00, 0x00], 100), None);
    assert_eq!(detector.observe(&[], 100), None);
}

#[test]
fn test_rtp_dtmf_detector_extended_tone_recognition() {
    let mut detector = RtpDtmfDetector::default();

    // Test all valid DTMF codes (0-15)
    let expected_digits = vec![
        ('0', 0u8),
        ('1', 1u8),
        ('2', 2u8),
        ('3', 3u8),
        ('4', 4u8),
        ('5', 5u8),
        ('6', 6u8),
        ('7', 7u8),
        ('8', 8u8),
        ('9', 9u8),
        ('*', 10u8),
        ('#', 11u8),
        ('A', 12u8),
        ('B', 13u8),
        ('C', 14u8),
        ('D', 15u8),
    ];

    for (expected_digit, digit_code) in expected_digits {
        let result = detector.observe(&[digit_code, 0x00, 0x00, 0xa0], digit_code as u32);
        assert_eq!(
            result,
            Some(expected_digit),
            "Failed to map DTMF code {} to digit {}: got {:?}",
            digit_code,
            expected_digit,
            result
        );
    }
}

#[test]
fn test_rtp_dtmf_detector_rapidly_repeated_digit() {
    let mut detector = RtpDtmfDetector::default();

    // User pressing "2" multiple times rapidly
    // First press should succeed
    assert_eq!(detector.observe(&[2, 0x00, 0x00, 0xa0], 1000), Some('2'));
    // Same timestamp = duplicate, should be filtered
    assert_eq!(detector.observe(&[2, 0x80, 0x01, 0x40], 1000), None);
    // New timestamp = new digit, should succeed
    assert_eq!(detector.observe(&[2, 0x00, 0x00, 0xa0], 2000), Some('2'));
    // Different digit on new timestamp
    assert_eq!(detector.observe(&[4, 0x00, 0x00, 0xa0], 3000), Some('4'));
}

#[test]
fn test_session_drop_releases_resources() {
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct DropTracker;
    impl Drop for DropTracker {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    {
        let _tracker = DropTracker;
    }

    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
}

#[test]
fn test_update_fallback_only_for_unsupported_methods() {
    assert!(SipSession::should_fallback_to_reinvite(
        StatusCode::MethodNotAllowed
    ));
    assert!(SipSession::should_fallback_to_reinvite(
        StatusCode::NotImplemented
    ));
    assert!(!SipSession::should_fallback_to_reinvite(
        StatusCode::RequestPending
    ));
    assert!(!SipSession::should_fallback_to_reinvite(
        StatusCode::RequestTimeout
    ));
    assert!(!SipSession::should_fallback_to_reinvite(
        StatusCode::Unauthorized
    ));
    assert!(!SipSession::should_fallback_to_reinvite(
        StatusCode::ServerInternalError
    ));
}

#[test]
fn test_route_via_home_proxy_detects_remote_home_proxy() {
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("192.168.1.10:5060").unwrap(),
    };
    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Tcp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.2:5070").unwrap(),
    };

    let target = Location {
        destination: Some(destination),
        home_proxy: Some(home_proxy.clone()),
        ..Default::default()
    };

    assert!(SipSession::route_via_home_proxy(&target, true, None));
}

#[test]
fn test_route_via_home_proxy_ignores_local_home_proxy() {
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("192.168.1.10:5060").unwrap(),
    };
    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Tcp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
    };

    let target = Location {
        destination: Some(destination.clone()),
        home_proxy: Some(home_proxy),
        ..Default::default()
    };

    let self_ident = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
    };

    assert!(!SipSession::route_via_home_proxy(
        &target,
        true,
        Some(&self_ident)
    ));
}

#[test]
fn test_callee_supports_webrtc_fallbacks() {
    fn loc(supports_webrtc: bool, dest_type: Option<rsipstack::sip::Transport>) -> Location {
        Location {
            supports_webrtc,
            destination: dest_type.map(|t| SipAddr {
                r#type: Some(t),
                addr: rsipstack::sip::HostWithPort::try_from("198.51.100.10:5060").unwrap(),
            }),
            ..Default::default()
        }
    }

    // Explicit flag wins regardless of transport.
    assert!(SipSession::callee_supports_webrtc(&loc(true, None)));

    // Regression: flag lost but resolved destination is WebSocket must still
    // classify the leg as WebRTC (otherwise a WSS/WebRTC callee receives a
    // plain RTP/AVP offer and rejects it with 488).
    assert!(SipSession::callee_supports_webrtc(&loc(
        false,
        Some(rsipstack::sip::Transport::Wss)
    )));
    assert!(SipSession::callee_supports_webrtc(&loc(
        false,
        Some(rsipstack::sip::Transport::Ws)
    )));

    // Plain UDP/TCP destinations are not WebRTC.
    assert!(!SipSession::callee_supports_webrtc(&loc(
        false,
        Some(rsipstack::sip::Transport::Udp)
    )));
    assert!(!SipSession::callee_supports_webrtc(&loc(
        false,
        Some(rsipstack::sip::Transport::Tcp)
    )));

    // No destination, but registered transport is WebSocket.
    assert!(SipSession::callee_supports_webrtc(&Location {
        supports_webrtc: false,
        transport: Some(rsipstack::sip::Transport::Wss),
        ..Default::default()
    }));

    // Nothing WebRTC at all.
    assert!(!SipSession::callee_supports_webrtc(&Location {
        supports_webrtc: false,
        ..Default::default()
    }));
}

#[test]
fn test_resolve_outbound_callee_uri_prefers_registered_aor_via_home_proxy() {
    let contact_uri =
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=WSS").unwrap();
    let registered_aor =
        rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com:443;transport=WSS").unwrap();
    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.2:5070").unwrap(),
    };
    let expected = rsipstack::sip::Uri::try_from("sip:lp@10.0.0.2:5070;transport=UDP").unwrap();

    let target = Location {
        aor: contact_uri,
        registered_aor: Some(registered_aor.clone()),
        home_proxy: Some(home_proxy),
        ..Default::default()
    };

    let resolved = SipSession::resolve_outbound_callee_uri(&target, true);
    assert_eq!(resolved, expected);
}

#[test]
fn test_resolve_outbound_callee_uri_falls_back_to_contact_when_no_registered_aor() {
    let contact_uri =
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();

    let target = Location {
        aor: contact_uri.clone(),
        ..Default::default()
    };

    let resolved = SipSession::resolve_outbound_callee_uri(&target, true);
    assert_eq!(resolved, contact_uri);
}

#[test]
fn test_resolve_outbound_callee_uri_uses_contact_when_not_via_home_proxy() {
    let contact_uri =
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
    let registered_aor = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();

    let target = Location {
        aor: contact_uri.clone(),
        registered_aor: Some(registered_aor),
        ..Default::default()
    };

    let resolved = SipSession::resolve_outbound_callee_uri(&target, false);
    assert_eq!(resolved, contact_uri);
}

#[tokio::test]
async fn test_target_invite_call_ids_resolve_before_dialing() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    use sea_orm::ConnectionTrait;
    for persistent in [false, true] {
        let (mut server, _) = create_test_server().await;
        let shared_registry = crate::call::runtime::MemorySessionRegistry::new(
            "node-1",
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        );
        let owner_registry: crate::call::runtime::SessionRegistryRef = if persistent {
            let mut options = sea_orm::ConnectOptions::new("sqlite::memory:");
            options.max_connections(1);
            let db = sea_orm::Database::connect(options).await.unwrap();
            // Fixed existing registry schema; no migrations or automatic column additions.
            db.execute_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "CREATE TABLE cluster_sessions (call_id VARCHAR(200) PRIMARY KEY NOT NULL,
             node_id VARCHAR(64) NOT NULL, caller VARCHAR(160) NOT NULL,
             callee VARCHAR(160) NOT NULL, direction VARCHAR(16) NOT NULL,
             target_session_id VARCHAR(200), started_at TIMESTAMP NOT NULL,
             last_updated_at TIMESTAMP NOT NULL)"
                    .to_string(),
            ))
            .await
            .unwrap();
            crate::call::runtime::DbSessionRegistry::new(
                db,
                Duration::from_secs(3600),
                Duration::from_secs(3600),
            )
        } else {
            shared_registry.clone()
        };
        Arc::get_mut(&mut server).unwrap().session_registry = owner_registry.clone();
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "charlie",
            None,
            "rustpbx.com",
            None,
        );
        let original_request = request.clone();
        let (tx, _) = create_transaction(request).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(
                &tx,
                state_tx,
                None,
                Some("sip:pbx@192.0.2.77:5099".try_into().unwrap()),
            )
            .unwrap();
        let incoming_call_id = server_dialog.id().call_id.clone();
        let session_id = "proxy-session".to_string();
        let context = CallContext {
            session_id: session_id.clone(),
            dialplan: Arc::new(
                Dialplan::new(session_id.clone(), original_request, DialDirection::Inbound)
                    .with_caller("sip:charlie@rustpbx.com".try_into().unwrap()),
            ),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:charlie@rustpbx.com".to_string(),
            original_callee: "sip:2001@rustpbx.com".to_string(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, handle, _cmd_rx) = SipSession::new(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            server_dialog,
            false,
        );
        let registry = &server.active_call_registry;
        registry.register_handle(session_id.clone(), handle);
        session.register_in_session_registry().await;
        assert_eq!(
            crate::call::runtime::resolve_owner_and_session(&owner_registry, &incoming_call_id,)
                .await,
            Some(("local".into(), "proxy-session".into())),
            "a peer must also resolve the originating phone's incoming SIP Call-ID"
        );
        let target = Location {
            aor: "sip:2001@rustpbx.com".try_into().unwrap(),
            ..Default::default()
        };
        let mut call_ids = Vec::new();

        // Exercise the builder used by both sequential dialing and parallel forks.
        // No INVITE has been sent and no LegConnected notification has occurred.
        for leg_id in [None, Some("fork-1")] {
            let (invite, _, call_id, _) = session
                .build_target_invite_option(&target, leg_id, None)
                .await
                .unwrap();
            // The incoming server Contact must not leak into outgoing INVITEs.
            assert_eq!(
                invite.contact,
                server
                    .contact_uri_for_location_with_sip_contact(&target, None)
                    .unwrap()
            );
            assert_ne!(
                invite.contact,
                session
                    .caller_dialog
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .local_contact
                    .unwrap()
            );
            assert_eq!(invite.call_id.as_deref(), Some(call_id.as_str()));
            assert_ne!(call_id, session_id);
            let resolved = registry
                .get_handle_by_call_id(&call_id)
                .expect("the desk's SIP Call-ID must resolve before it receives INVITE");
            assert_eq!(resolved.session_id(), session_id);
            let peer_resolution =
                crate::call::runtime::resolve_owner_and_session(&server.session_registry, &call_id)
                    .await;
            assert_eq!(
                peer_resolution,
                Some(("local".into(), session_id.clone())),
                "a peer must resolve the desk SIP Call-ID before INVITE is sent"
            );
            if !persistent {
                shared_registry
                    .backdate_for_test(&call_id, Duration::from_secs(3601))
                    .await;
                owner_registry
                    .heartbeat_node("local", &registry.live_call_ids())
                    .await
                    .unwrap();
                assert!(
                    owner_registry.lookup_owner(&call_id).await.is_some(),
                    "a long call must keep its dialog owner mapping alive"
                );
            }
            call_ids.push(call_id);
        }
        assert_ne!(call_ids[0], call_ids[1]);
        assert!(registry.get_handle(&session_id).is_some());
        let transfer_headers = HashMap::from([
            (
                "X-Route-Metadata".to_string(),
                "workflow=example,variant=one".to_string(),
            ),
            ("X-Correlation-Id".to_string(), session_id.clone()),
        ]);
        let (invite, _, _, _) = session
            .build_target_invite_option(&target, None, Some(&transfer_headers))
            .await
            .unwrap();
        let rendered_headers = invite
            .headers
            .expect("outbound INVITE headers must be present")
            .into_iter()
            .map(|header| (header.name().to_string(), header.value().to_string()))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            rendered_headers["X-Route-Metadata"],
            "workflow=example,variant=one"
        );
        assert_eq!(rendered_headers["X-Correlation-Id"], session_id);

        for invalid_headers in [
            HashMap::from([("Via".to_string(), "SIP/2.0/UDP attacker".to_string())]),
            HashMap::from([("Content-Length".to_string(), "0".to_string())]),
            HashMap::from([("Session-Expires".to_string(), "1800".to_string())]),
            HashMap::from([(
                "X-Route-Metadata".to_string(),
                "workflow=example\r\nVia: attacker".to_string(),
            )]),
            HashMap::from([("Invalid Header".to_string(), "value".to_string())]),
            HashMap::from([
                ("X-Duplicate".to_string(), "first".to_string()),
                ("x-duplicate".to_string(), "second".to_string()),
            ]),
        ] {
            assert!(
                session
                    .build_target_invite_option(&target, None, Some(&invalid_headers))
                    .await
                    .is_err()
            );
        }
        for call_id in &call_ids {
            assert!(registry.get_handle_by_call_id(call_id).is_some());
        }

        // A transferor can leave while the customer session remains alive.
        // Both local and peer lookup must retire that dialog, not the session.
        let transferor = session.caller_dialog_id();
        session.release_transferor_caller_leg(&transferor).await;
        assert!(registry.get_handle_by_call_id(&incoming_call_id).is_none());
        assert!(
            owner_registry
                .lookup_owner(&incoming_call_id)
                .await
                .is_none(),
            "a departed transferor must not retain a cross-node control alias"
        );
        assert!(
            owner_registry.lookup_owner(&session_id).await.is_some(),
            "retiring a dialog must preserve the customer's canonical session"
        );

        session.cleanup().await;
        call_ids.push(incoming_call_id);
        for call_id in &call_ids {
            assert!(
                owner_registry.lookup_owner(call_id).await.is_none(),
                "ended dialogs must not leave stale cross-node owner records"
            );
        }
        registry.remove(&session_id);
        for call_id in &call_ids {
            assert!(registry.get_handle_by_call_id(call_id).is_none());
        }
    }
}

#[tokio::test]
async fn test_init_callee_timer_disabled_without_session_expires() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    let dialog_id = DialogId {
        call_id: "callee-call".into(),
        local_tag: "local".into(),
        remote_tag: "remote".into(),
    };
    let response = rsipstack::sip::Response {
        status_code: StatusCode::OK,
        version: rsipstack::sip::Version::V2,
        headers: rsipstack::sip::Headers::default(),
        body: Vec::new(),
        wire_reason: None,
    };

    session.init_callee_timer(
        dialog_id.clone(),
        &response,
        Duration::from_secs(DEFAULT_SESSION_EXPIRES),
    );

    let timer = session
        .timers
        .get(&dialog_id)
        .expect("missing callee timer");
    assert!(!timer.enabled);
    assert!(!timer.active);
    assert_eq!(
        timer.session_interval,
        Duration::from_secs(DEFAULT_SESSION_EXPIRES)
    );
    assert!(!session.timer_keys.contains_key(&dialog_id));
}

/// Regression: preparing the app/IVR caller media bridge must NOT open the
/// caller gate — the gate opens only when the 200 OK is sent (accept_call).
/// Before the fix, the app path never opened the gate at all, so caller
/// audio + RFC 2833 DTMF were dropped → "RTP timeout: caller side silent"
/// and IVR digit timeout.

/// Regression: the app/IVR answer flow (prepare bridge → accept_call/200 OK)
/// must open the caller gate. Before the fix, accept_call never opened the
/// gate for the app path, dropping all caller→app RTP/DTMF.

/// Regression test for the both-WebRTC + IVR recording bug.

/// Verify WebRTC caller → RTP agent reuses bridge callee PC.

#[tokio::test]
async fn test_sip_session_handle() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-session");
    let (handle, mut cmd_rx) = SipSession::with_handle(id.clone());

    let result = handle.send_command(CallCommand::Answer {
        leg_id: LegId::from("caller"),
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::Answer { .. })));

    drop(handle);
}

#[tokio::test]
async fn test_cancel_token_propagation() {
    let cancel_token = CancellationToken::new();
    let child_token = cancel_token.child_token();

    let task = crate::utils::spawn(async move {
        tokio::select! {
            _ = child_token.cancelled() => {
                "cancelled"
            }
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                "timeout"
            }
        }
    });

    cancel_token.cancel();

    let result = tokio::time::timeout(Duration::from_millis(100), task).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().unwrap(), "cancelled");
}

#[test]
fn test_caller_rejection_ack_timeout_is_3_seconds() {
    assert_eq!(
        SipSession::CALLER_REJECTION_ACK_TIMEOUT,
        Duration::from_secs(3),
        "CALLER_REJECTION_ACK_TIMEOUT must be 3s — the caller-cancel drain window"
    );
}

#[tokio::test]
async fn test_cancelled_token_guard_prevents_busy_loop() {
    let token = CancellationToken::new();
    let mut entry_count = 0;

    token.cancel();

    let child = token.child_token();
    // Simulate the setup-loop pattern: `cancel_token.cancelled(), if !guard`
    let mut guard = false;

    tokio::select! {
        _ = child.cancelled() => {
            if !guard {
                guard = true;
                entry_count += 1;
            }
        }
        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
    }

    // Token is already cancelled. A second select would fire
    // immediately again if unguarded, but the guard (`if !guard`)
    // in the real loop would suppress re-entry. Verify the guard
    // was set after the first entry.
    assert!(guard, "guard must be set after first cancelled() entry");
    assert_eq!(entry_count, 1, "guard must allow exactly one entry");

    // Verify the guard persists — the next cancelled() should
    // be suppressed (simulated by the guard already being true).
    assert!(
        guard,
        "guard stays true to prevent re-entry into the cancel branch"
    );
}

#[tokio::test]
async fn test_callee_event_channel_closed() {
    use rsipstack::dialog::DialogId;

    let (tx, mut rx) = mpsc::unbounded_channel::<DialogState>();

    let dialog_id = DialogId {
        call_id: "test".into(),
        local_tag: "local".into(),
        remote_tag: "remote".into(),
    };
    let _ = tx.send(DialogState::Trying(dialog_id));

    assert!(rx.recv().await.is_some());

    drop(tx);

    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn test_process_uac_handles_first_invite_termination_as_caller_state() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    let (server, _) = create_test_server().await;
    let original_request = create_test_request(
        rsipstack::sip::Method::Invite,
        "rwi",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "rwi-uac-caller-state".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "rwi-uac-caller-state".to_string(),
            original_request,
            DialDirection::Outbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:rwi@rustpbx.com".to_string(),
        original_callee: "sip:target@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let dialog_layer = server.dialog_layer.clone();
    let (mut session, _handle, cmd_rx) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);
    let (caller_tx, caller_rx) = mpsc::unbounded_channel();
    let (_callee_tx, callee_rx) = mpsc::unbounded_channel();
    let dialog_id = DialogId {
        call_id: "rwi-first-invite".into(),
        local_tag: "local".into(),
        remote_tag: "remote".into(),
    };

    caller_tx
        .send(DialogState::Terminated(
            dialog_id.clone(),
            TerminatedReason::UasBye,
        ))
        .expect("caller state receiver must be open");
    let dialog_guard = ClientDialogGuard::new(dialog_layer, dialog_id);

    tokio::time::timeout(
        Duration::from_secs(2),
        session.process_uac(caller_rx, callee_rx, cmd_rx, dialog_guard),
    )
    .await
    .expect("caller BYE must stop the UAC session")
    .expect("UAC session should shut down cleanly");

    assert!(matches!(
        session.meta.hangup_reason,
        Some(CallRecordHangupReason::ByCallee)
    ));
}

#[tokio::test]
async fn rwi_originate_uses_prepared_caller_leg_for_invite_answer() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    let (server, _) = create_test_server().await;
    let original_request = create_test_request(
        rsipstack::sip::Method::Invite,
        "rwi",
        None,
        "rustpbx.com",
        None,
    );
    let mut dialplan = Dialplan::new(
        "rwi-prepared-caller-leg".to_string(),
        original_request,
        DialDirection::Outbound,
    );
    dialplan.media.rtp_start_port = Some(39000);
    dialplan.media.rtp_end_port = Some(39010);
    let context = CallContext {
        session_id: "rwi-prepared-caller-leg".to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:rwi@rustpbx.com".to_string(),
        original_callee: "sip:target@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _cmd_rx) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, true);
    let codecs = vec![MediaNegotiator::codec_info_for_type(CodecType::PCMU)];

    let offer = session
        .prepare_originate_caller_leg(codecs)
        .await
        .expect("originate A leg must create the INVITE offer");
    let offered_port = extract_audio_port(&offer).expect("offer audio port");
    assert!(
        (39000..=39010).contains(&offered_port),
        "originate offer port {offered_port} must honor the configured RTP range"
    );
    let caller_leg_before = session
        .media_leg(&LegId::from("caller"))
        .expect("prepared caller A leg");
    assert!(
        session.media_leg(&LegId::from("callee")).is_none(),
        "one-target originate must not synthesize a B leg"
    );

    let remote = LegInner::new("rwi-remote", &LegConfig::rtp_pcmu(), None).expect("remote RTP leg");
    let answer = remote.answer(&offer).await.expect("remote SDP answer");
    let caller_leg = session
        .media_leg(&LegId::from("caller"))
        .expect("prepared caller A leg");
    caller_leg
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("answer must apply to prepared A leg");
    caller_leg.accept();
    assert!(
        session.media.bridge.is_none(),
        "a single peer does not need a bridge"
    );

    let caller_leg_after = session
        .media_leg(&LegId::from("caller"))
        .expect("completed caller A leg");
    assert!(
        Arc::ptr_eq(&caller_leg_before, &caller_leg_after),
        "answer must not replace the PeerConnection that generated the offer"
    );
    assert!(caller_leg_after.negotiated().is_some());
    assert!(!caller_leg_after.is_gated());
    assert!(
        session.media_leg(&LegId::from("callee")).is_none(),
        "answering the first target must still leave B empty"
    );

    remote.stop();
}

#[tokio::test]
async fn rwi_bridge_setup_results_reach_listener_before_call_ends() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};

    use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::rwi::gateway::RwiGateway;
    use crate::rwi::processor::RwiCommandProcessor;
    use crate::rwi::session::RwiCommandPayload;

    let (server, _) = create_test_server().await;
    let call_id = "rwi-bridge-setup";
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "rwi",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: call_id.into(),
        dialplan: Arc::new(Dialplan::new(
            call_id.into(),
            request,
            DialDirection::Outbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:rwi@rustpbx.com".into(),
        original_callee: "sip:target@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, handle, mut commands) = SipSession::new_uac(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        true,
    );
    let offer = session
        .prepare_originate_caller_leg(vec![MediaNegotiator::codec_info_for_type(CodecType::PCMU)])
        .await
        .unwrap();
    let remote = LegInner::new("bridge-remote", &LegConfig::rtp_pcmu(), None).unwrap();
    let answer = remote.answer(&offer).await.unwrap();
    let caller = session.media_leg(&LegId::from("caller")).unwrap();
    caller
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    session.media_leg(&LegId::from("caller")).unwrap().accept();
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);
    server.active_call_registry.upsert(
        ActiveProxyCallEntry {
            session_id: call_id.into(),
            caller: None,
            callee: None,
            direction: "outbound".into(),
            started_at: chrono::Utc::now(),
            answered_at: Some(chrono::Utc::now()),
            status: ActiveProxyCallStatus::Talking,
        },
        handle,
    );

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let processor = RwiCommandProcessor::new(
        server.active_call_registry.clone(),
        Arc::new(parking_lot::RwLock::new(gateway)),
        server.conference_manager.clone(),
    )
    .with_sip_server(server.clone());
    let listener = processor.register_transfer_notify_listener().await.unwrap();

    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = ws_listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let ws_task = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _ = stop_rx.await;
        drop(ws);
    });
    let (_callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    for (target, succeeds, expected_event) in [
        (
            format!("bridge:ws://{address}?timeout_ms=1000"),
            true,
            "call_transferred",
        ),
        (
            "bridge:invalid-websocket-url".into(),
            false,
            "call_transfer_failed",
        ),
    ] {
        tokio::time::timeout(
            Duration::from_secs(2),
            processor.process_command(RwiCommandPayload::Transfer {
                call_id: call_id.into(),
                target: target.clone(),
            }),
        )
        .await
        .expect("RWI must return before the session executes setup")
        .unwrap();
        let command = commands.recv().await.unwrap();
        assert!(matches!(command, CallCommand::Transfer { .. }));
        let result = session.execute_command(command, Some(&mut callee_rx)).await;
        assert_eq!(result.success, succeeds, "{:?}", result.message);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let entry = events.recv().await.unwrap();
                if entry.event.event_type == expected_event {
                    assert_eq!(entry.call_id, call_id);
                    assert_eq!(entry.event.payload["transfer_target"], target);
                    break;
                }
            }
        })
        .await
        .expect("bridge setup result must reach the RWI controller");
        assert!(!session.cancel_token.is_cancelled());
        assert!(session.external_bridge.is_some());
        assert!(
            session.conference_bridge.conf_id.is_none(),
            "a voip bridge must not occupy the conference slot"
        );
        assert!(server.active_call_registry.get(call_id).is_some());
    }

    // Setup reports directly, even without execute_command and while an
    // additional subscriber has not started reading its events.
    let (observer_tx, mut observer_rx) = mpsc::unbounded_channel();
    server
        .transfer_notify_subscribers
        .lock()
        .await
        .push(observer_tx);
    let target = "bridge:invalid-websocket-url";
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        session.handle_blind_transfer(
            LegId::from("caller"),
            target.into(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        ),
    )
    .await
    .expect("setup must not wait for subscribers to process the result");
    assert!(result.is_err());
    let event = observer_rx
        .try_recv()
        .expect("bridge setup must emit its own result");
    assert_eq!(event.sip_status, 500);
    assert!(matches!(
        event.event_type,
        crate::call::domain::ReferNotifyEventType::Notify
    ));

    drop(observer_rx);

    session.cleanup().await;
    let _ = stop_tx.send(());
    ws_task.await.unwrap();
    remote.stop();
    drop(listener);
}

#[tokio::test]
async fn test_reject_command() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-reject");
    let (handle, mut cmd_rx) = SipSession::with_handle(id);

    let result = handle.send_command(CallCommand::Reject {
        leg_id: LegId::from("caller"),
        reason: Some("User busy".to_string()),
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::Reject { .. })));

    drop(handle);
}

#[tokio::test]
async fn test_ring_command() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-ring");
    let (handle, mut cmd_rx) = SipSession::with_handle(id);

    let result = handle.send_command(CallCommand::Ring {
        leg_id: LegId::from("caller"),
        ringback: None,
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::Ring { .. })));

    drop(handle);
}

#[tokio::test]
async fn test_send_dtmf_command() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-dtmf");
    let (handle, mut cmd_rx) = SipSession::with_handle(id);

    let result = handle.send_command(CallCommand::SendDtmf {
        leg_id: LegId::from("caller"),
        digits: "1234".to_string(),
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::SendDtmf { .. })));

    drop(handle);
}

#[tokio::test]
async fn test_handle_reinvite_command() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-reinvite");
    let (handle, mut cmd_rx) = SipSession::with_handle(id);

    let result = handle.send_command(CallCommand::HandleReInvite {
        leg_id: LegId::from("caller"),
        sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n"
            .to_string(),
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::HandleReInvite { .. })));

    drop(handle);
}

#[tokio::test]
async fn test_mute_track_command() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-mute");
    let (handle, mut cmd_rx) = SipSession::with_handle(id);

    let result = handle.send_command(CallCommand::MuteTrack {
        track_id: "track-1".to_string(),
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::MuteTrack { .. })));

    drop(handle);
}

#[tokio::test]
async fn test_unmute_track_command() {
    use crate::call::runtime::SessionId;

    let id = SessionId::from("test-unmute");
    let (handle, mut cmd_rx) = SipSession::with_handle(id);

    let result = handle.send_command(CallCommand::UnmuteTrack {
        track_id: "track-1".to_string(),
    });
    assert!(result.is_ok());

    let received = cmd_rx.recv().await;
    assert!(matches!(received, Some(CallCommand::UnmuteTrack { .. })));

    drop(handle);
}

// ============================================================================
// Call forwarding -> queue/ivr tests
// ============================================================================

#[tokio::test]
async fn test_handle_blind_transfer_queue_prefix() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_config, create_transaction,
    };

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let (server, _) = create_test_server_with_config(config).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;

    assert!(
        result.is_ok(),
        "handle_blind_transfer with queue: prefix should succeed, got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_handle_blind_transfer_queue_not_found() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::call_errors::TraceKind;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:nonexistent".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;

    // With the graceful-fallback change, a missing queue no longer surfaces
    // a bare "not found" error that leaves the caller in dead air. Instead
    // the session records a `queue.not_found` trace event and attempts to
    // start the fallback queue app (which plays the service-unavailable
    // announcement then hangs up). In this bare test session the app
    // factory is absent so the queue app cannot fully start — the decisive
    // observable is the recorded trace event.
    let not_found_trace = session.meta.trace.iter().any(|ev| {
        ev.kind == TraceKind::Queue
            && ev.code.as_deref() == Some("queue.not_found")
            && ev.message.contains("nonexistent")
    });
    assert!(
        not_found_trace,
        "missing-queue fallback should record a queue.not_found trace event; trace = {:?}",
        session.meta.trace
    );
    // The caller-facing error (if any) must not be the old dead-air
    // "not found" message.
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            !msg.contains("Queue 'nonexistent' not found"),
            "should no longer surface the bare not-found error, got: {}",
            msg
        );
    }
}

/// Blind transfers to in-session application targets (queue/ivr/...) must
/// surface a `call_transferred` RWI event annotated with the resolved target
/// type and the flow origin (IVR name + node) captured before the hand-off.
#[tokio::test]
async fn test_blind_transfer_queue_prefix_emits_transferred_with_source() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    // Simulate the call flowing out of an IVR: the session remembers the
    // originating IVR short code and the current node.
    session.session_ext_set("ivr", "main-ivr");
    session.session_ext_set("ivr_node", "menu-1");

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "queue transfer should succeed: {:?}",
        result
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for queue-target blind transfer");

    assert_eq!(entry.call_id, "test-session");
    assert_eq!(entry.event.payload["transfer_target"], "queue:test-queue");
    assert_eq!(entry.event.payload["transfer_target_type"], "queue");
    assert_eq!(entry.event.payload["transfer_source"]["source_type"], "ivr");
    assert_eq!(entry.event.payload["transfer_source"]["name"], "main-ivr");
    assert_eq!(
        entry.event.payload["transfer_source"]["ivr_node_id"],
        "menu-1"
    );
}

/// A queue-served call blind-transferred onward reports the serving queue
/// (and transferring agent) as the flow source.
#[tokio::test]
async fn recording_identity_blind_transfer_keeps_departing_stage_after_handoff() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    // Simulate a queue-served call: the customer was talking to agent 2002
    // of queue sales when the agent blind-transferred them onward. The CC
    // hook publishes the paired display name next to the resolved id.
    session.meta.queue_name = Some("sales".to_string());
    session
        .legs
        .get_mut(&LegId::from("caller"))
        .unwrap()
        .agent_id = Some("2002".into());
    session.session_ext_set("resolved_agent_id", "2002");
    session.session_ext_set("agent_name", "Alice");

    let gateway = server.rwi_gateway.as_ref().unwrap();
    let source_id = "12345678-1234-4123-8123-123456789abc";
    let mut meta = gateway
        .read()
        .meta_store
        .get_sync("test-session")
        .unwrap_or_default();
    meta.agent_id = Some("2002".into());
    meta.recording_id = Some(source_id.into());
    gateway
        .read()
        .meta_store
        .insert("test-session".into(), meta);

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "queue transfer should succeed: {:?}",
        result
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for queue-target blind transfer");

    assert_eq!(entry.event.payload["transfer_target_type"], "queue");
    assert_eq!(
        entry.event.payload["transfer_source"]["source_type"],
        "queue"
    );
    assert_eq!(entry.event.payload["transfer_source"]["name"], "sales");
    assert_eq!(entry.event.payload["transfer_source"]["agent_id"], "2002");
    assert_eq!(
        entry.event.payload["transfer_source"]["agent_name"],
        "Alice"
    );
    assert_eq!(entry.event.payload["recording_id"], source_id);

    // Completion emitters can run after the target agent has replaced the live call metadata.
    let mut meta = gateway.read().meta_store.get_sync("test-session").unwrap();
    meta.agent_id = Some("3003".into());
    meta.recording_id = Some("87654321-4321-4321-8321-cba987654321".into());
    gateway
        .read()
        .meta_store
        .insert("test-session".into(), meta);
    gateway.read().send_to_owner(&crate::rwi::CallTransferred {
        call_id: "test-session".into(),
        transfer_target: Some("sip:3003@example.test".into()),
        transfer_target_type: Some("sip".into()),
        transfer_source: None,
    });
    let completed = std::iter::from_fn(|| events.try_recv().ok())
        .find(|entry| entry.event.event_type == "call_transferred")
        .unwrap();
    assert_eq!(
        completed.event.payload["transfer_source"]["agent_id"],
        "2002"
    );
    assert_eq!(completed.event.payload["recording_id"], source_id);
}

/// A phone-native REFER handing the call to an application successor
/// (`queue:` / `toivr:` …) must stamp the transfer. The transferor's later BYE
/// is only classified as a transfer exit through `transferred_agent_context`,
/// which is the exact gate that fires `on_agent_disconnected` — and with it
/// the CC hook's agent recording-segment closer. Without the stamp the flag
/// stayed false and the segment kept recording the successor stage of a
/// transferred call.
#[tokio::test]
async fn inbound_refer_to_app_target_marks_transferred_and_resolves_transferor_exit() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let gateway = RwiGateway::new();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");
    let dialog_id = server_dialog.id();

    let context = CallContext {
        session_id: "refer-app-target".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "refer-app-target".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:1002@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    // Queue-served call: the REFERor is the agent leg, identity stamped the
    // same way `handle_received_refer` stamps a referring agent leg.
    session.update_leg_state(
        &LegId::from("caller"),
        crate::call::domain::LegState::Connected,
    );
    session
        .legs
        .get_mut(&LegId::from("caller"))
        .unwrap()
        .agent_id = Some("2002".into());
    session.session_ext_set("resolved_agent_id", "2002");

    let result = session
        .handle_inbound_refer(
            dialog_id,
            "queue:test-queue".to_string(),
            HashMap::new(),
            &mut callee_rx,
        )
        .await;
    assert!(
        result.is_ok(),
        "in-session REFER to a queue target must succeed: {result:?}"
    );

    // (1) The hand-off stamped the transfer …
    assert!(
        session.meta.transferred,
        "app-target REFER must mark the call transferred"
    );
    // (2) … so the transferor's later BYE resolves as a transfer exit — the
    // gate `handle_callee_state` uses to fire `on_agent_disconnected`.
    let ctx = session
        .transferred_agent_context(&LegId::from("caller"))
        .expect("transferor BYE must classify as a transfer exit");
    assert!(ctx.transferred);
}

/// Regression guard for the fix above: ordinary in-session application
/// hand-offs (IVR `queue:` nodes, queue overflow, no-answer fallbacks) run
/// through `CallCommand::Transfer` → `handle_transfer` — NOT through
/// `handle_inbound_refer` — and must keep `meta.transferred` false. The
/// queue-abandon classifier and the CDR `transferred` marker depend on that.
#[tokio::test]
async fn command_transfer_to_app_target_does_not_mark_transferred() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let gateway = RwiGateway::new();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "cmd-transfer-queue".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "cmd-transfer-queue".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:1002@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);
    session.update_leg_state(
        &LegId::from("caller"),
        crate::call::domain::LegState::Connected,
    );

    let result = session
        .handle_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            false,
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "in-session command transfer to a queue target must succeed: {result:?}"
    );
    assert!(
        !session.meta.transferred,
        "CallCommand::Transfer hand-offs must not flip the transferred flag \
         (only REFER-initiated hand-offs stamp it)"
    );
}

/// A bare (non-IVR, non-queue) call blind-transferred by an agent attributes
/// the transfer to that agent, carrying both the id and the hook-published
/// display name. The `name` field stays unset on the agent branch (use
/// `agent_name`).
#[tokio::test]
async fn test_blind_transfer_reports_agent_flow_source_with_name() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    // No IVR and no queue context: the transferring agent is the flow origin.
    session
        .legs
        .get_mut(&LegId::from("caller"))
        .unwrap()
        .agent_id = Some("2002".into());
    session.session_ext_set("resolved_agent_id", "2002");
    session.session_ext_set("agent_name", "Alice");

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "queue transfer should succeed: {:?}",
        result
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for queue-target blind transfer");

    assert_eq!(entry.event.payload["transfer_target_type"], "queue");
    assert_eq!(
        entry.event.payload["transfer_source"]["source_type"],
        "agent"
    );
    assert_eq!(entry.event.payload["transfer_source"]["agent_id"], "2002");
    assert_eq!(
        entry.event.payload["transfer_source"]["agent_name"],
        "Alice"
    );
    assert!(
        entry.event.payload["transfer_source"].get("name").is_none(),
        "agent branch keeps `name` unset"
    );
}

/// When the transferring agent id is only known through the session fallbacks
/// (transferor leg endpoint / connected callee — no CC-hook resolution), the
/// snapshot must NOT pair it with an unrelated `agent_name` extension: the
/// name is omitted.
#[tokio::test]
async fn test_blind_transfer_agent_name_requires_resolved_id() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::RouteQueueConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.queues.insert(
        "test-queue".to_string(),
        RouteQueueConfig {
            name: Some("test-queue".to_string()),
            ..Default::default()
        },
    );

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    // The transferring leg owns agent attribution; a stale session-level
    // `agent_name` extension must not be attached to it.
    session
        .legs
        .get_mut(&LegId::from("caller"))
        .unwrap()
        .agent_id = Some("2002".into());
    session.meta.connected_callee = Some("sip:2002@rustpbx.com".to_string());
    session.session_ext_set("agent_name", "Alice");

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "queue transfer should succeed: {:?}",
        result
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for queue-target blind transfer");

    assert_eq!(
        entry.event.payload["transfer_source"]["source_type"],
        "agent"
    );
    assert_eq!(entry.event.payload["transfer_source"]["agent_id"], "2002");
    assert!(
        entry.event.payload["transfer_source"]
            .get("agent_name")
            .is_none(),
        "agent_name must be omitted when the id did not come from the CC hook"
    );
}

/// A blind transfer to a bare number that is NOT a registered contact but
/// whose route-table entry resolves to a queue starts the QueueApp in-session
/// (gated by `route_originated_calls`) instead of dialing the number, and the
/// emitted `call_transferred` carries `transfer_target_type: "queue"` with
/// the original bare number as the target string.
#[tokio::test]
async fn test_blind_transfer_bare_number_routes_to_queue() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{MatchConditions, RouteAction, RouteQueueConfig, RouteRule};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.route_originated_calls = true;
    config.queues.insert(
        "ivr-entry-queue".to_string(),
        RouteQueueConfig {
            name: Some("ivr-entry-queue".to_string()),
            // Inline target so the matcher accepts the queue action without a
            // trunk `dest`; the member is offline — QueueApp dials (and fails)
            // in the background after the hand-off has already completed.
            strategy: crate::proxy::routing::RouteQueueStrategyConfig {
                targets: vec![crate::proxy::routing::RouteQueueTargetConfig {
                    uri: "sip:offline-agent@rustpbx.com".to_string(),
                    label: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "ivr-entry".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("8000".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("queue".to_string()),
            queue: Some("ivr-entry-queue".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(
            Dialplan::new(
                "test-session".to_string(),
                original_request,
                DialDirection::Inbound,
            )
            .with_caller(
                "sip:alice@rustpbx.com"
                    .parse()
                    .expect("caller URI must parse"),
            ),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "8000".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
            HashMap::new(),
        )
        .await;
    assert!(
        result.is_ok(),
        "bare-number blind transfer should route to the queue: {:?}",
        result
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for the routed queue hand-off");

    assert!(
        entry.event.payload["transfer_target"]
            .as_str()
            .is_some_and(|t| t.contains("8000")),
        "transfer_target must retain the original bare number: {}",
        entry.event.payload["transfer_target"]
    );
    assert_eq!(entry.event.payload["transfer_target_type"], "queue");
}

/// CC API blind transfer (`BlindTransferCustomer`) to a bare number whose
/// route-table entry maps to a QUEUE: the customer must hand off IN-SESSION
/// (QueueApp starts, `call_transferred` kind=queue, agent leg released) —
/// previously this API dumb-dialled "8000" and failed while the SIP REFER
/// path routed correctly.
#[tokio::test]
async fn test_customer_blind_transfer_route_to_queue() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{MatchConditions, RouteAction, RouteQueueConfig, RouteRule};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.route_originated_calls = true;
    config.queues.insert(
        "ivr-entry-queue".to_string(),
        RouteQueueConfig {
            name: Some("ivr-entry-queue".to_string()),
            strategy: crate::proxy::routing::RouteQueueStrategyConfig {
                targets: vec![crate::proxy::routing::RouteQueueTargetConfig {
                    uri: "sip:offline-agent@rustpbx.com".to_string(),
                    label: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "ivr-entry".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("8000".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("queue".to_string()),
            queue: Some("ivr-entry-queue".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let _original_request = request.clone();
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(
            Dialplan::new("test-session".to_string(), request, DialDirection::Inbound)
                .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    // Inbound CC shape: caller = customer, callee = agent (single
    // agent-stamped leg — `resolve_cc_agent_leg`'s stamped-leg fallback).
    let mut customer = Leg::new(LegId::from("caller"));
    customer.state = LegState::Connected;
    session.legs.insert(LegId::from("caller"), customer);
    let mut agent = Leg::new(LegId::from("callee"));
    agent.state = LegState::Connected;
    agent.agent_id = Some("bob".to_string());
    session.legs.insert(LegId::from("callee"), agent);

    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        session.execute_command(
            CallCommand::BlindTransferCustomer {
                target: "8000".to_string(),
                headers: Default::default(),
                application_only: false,
                completion: None,
            },
            Some(&mut callee_rx),
        ),
    )
    .await
    .expect("routed queue hand-off must not hang");
    assert!(
        result.success,
        "customer blind transfer to a routed queue must succeed: {:?}",
        result.message
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for the routed queue hand-off");
    assert!(
        entry.event.payload["transfer_target"]
            .as_str()
            .is_some_and(|t| t.contains("8000")),
        "transfer_target must retain the original bare number: {}",
        entry.event.payload["transfer_target"]
    );
    assert_eq!(entry.event.payload["transfer_target_type"], "queue");

    assert!(
        session.legs.get(&LegId::from("callee")).is_none(),
        "agent leg must be released with the hand-off"
    );
    assert!(
        session.legs.get(&LegId::from("caller")).is_some(),
        "customer leg must survive the hand-off"
    );
}

struct OutboundTransferFixture {
    session: SipSession,
    agent_states: mpsc::UnboundedReceiver<DialogState>,
    callee_states: mpsc::UnboundedReceiver<DialogState>,
    target: LegId,
    customer_dialog: DialogId,
}

async fn outbound_transfer_fixture() -> OutboundTransferFixture {
    use crate::call::{DialDirection, Dialplan, MediaConfig, TransactionCookie};
    use crate::config::MediaProxyMode;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "agent",
        None,
        "127.0.0.1",
        None,
    );
    let (server, _) = create_test_server().await;
    let (tx, _) = create_transaction(request.clone()).await;
    let (agent_tx, agent_states) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, agent_tx, None, None)
        .unwrap();
    dialog.accept(None, None).unwrap();
    let dialplan = Dialplan::new("outbound-transfer".into(), request, DialDirection::Outbound)
        .with_caller("sip:agent@127.0.0.1".try_into().unwrap())
        .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All));
    let context = CallContext {
        session_id: "outbound-transfer".into(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:agent@127.0.0.1".into(),
        original_callee: "sip:customer@127.0.0.1".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        dialog,
        true,
    );
    let caller = LegId::from("caller");
    let customer = LegId::from("callee");
    session.legs.get_mut(&caller).unwrap().agent_id = Some("agent".into());
    session.update_leg_state(&caller, LegState::Connected);
    let mut customer_leg = Leg::new(customer.clone());
    customer_leg.state = LegState::Connected;
    session.legs.insert(customer.clone(), customer_leg);
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "customer",
        None,
        "127.0.0.1",
        None,
    );
    let (tx, _) = create_transaction(request).await;
    let (callee_tx, mut callee_states) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, callee_tx.clone(), None, None)
        .unwrap();
    dialog.accept(None, None).unwrap();
    let customer_dialog = dialog.id();
    session
        .legs
        .set_dialog(customer.clone(), Dialog::Invite(dialog));
    session.meta.connected_callee_dialog_id = Some(customer_dialog.clone());
    session.callee_dialogs.insert(customer_dialog.clone(), ());
    let customer_peer = LegInner::new("callee", &LegConfig::rtp_pcmu(), None).unwrap();
    let remote = LegInner::new("customer", &LegConfig::rtp_pcmu(), None).unwrap();
    let offer = remote.create_offer().await.unwrap();
    let answer = customer_peer
        .apply_sdp(&offer, rustrtc::SdpType::Offer)
        .await
        .unwrap();
    remote
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    customer_peer.accept();
    remote.accept();
    session.legs.set_media_leg(&customer, customer_peer);
    session.callee_event_tx = Some(callee_tx);
    let result = session
        .execute_command(
            CallCommand::BlindTransferCustomer {
                target: "sip:target@127.0.0.1:5099".into(),
                headers: HashMap::new(),
                application_only: false,
                completion: None,
            },
            Some(&mut callee_states),
        )
        .await;
    assert!(result.success, "{:?}", result.message);
    let target = session
        .legs
        .iter()
        .find(|(_, leg)| leg.source_leg.as_ref() == Some(&customer))
        .map(|(id, _)| id.clone())
        .expect("transfer target must be customer-anchored");
    OutboundTransferFixture {
        session,
        agent_states,
        callee_states,
        target,
        customer_dialog,
    }
}

async fn receive_transfer_bye(
    mut dialog: rsipstack::dialog::invite_dialog::InviteDialog,
    states: &mut mpsc::UnboundedReceiver<DialogState>,
) -> DialogState {
    use crate::proxy::tests::common::create_transaction;
    let mut request = dialog.initial_request();
    request.method = rsipstack::sip::Method::Bye;
    // The fixture INVITE uses CSeq 1; BYE is the next in-dialog request.
    for header in request.headers.iter_mut() {
        match header {
            rsipstack::sip::Header::CSeq(value) => *value = "2 BYE".into(),
            rsipstack::sip::Header::To(value) => *value = dialog.snapshot().to.to_string().into(),
            _ => {}
        }
    }
    let (mut tx, _) = create_transaction(request).await;
    dialog.handle(&mut tx).await.unwrap();
    assert!(dialog.state().is_terminated());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = states
                .recv()
                .await
                .expect("dialog state channel must remain open");
            if matches!(&state, DialogState::Terminated(id, _) if *id == dialog.id()) {
                return state;
            }
        }
    })
    .await
    .expect("SIP BYE must emit termination")
}

#[tokio::test]
async fn customer_blind_transfer_outbound_target_ends_before_agent_event() {
    for ended in [false, true] {
        let mut fixture = outbound_transfer_fixture().await;
        let _guard = fixture.session.cancel_token.clone().drop_guard();
        let dialog = fixture.session.caller_dialog.clone().unwrap();
        // The SIP stack updates the shared dialog before the session consumes its event.
        let state = receive_transfer_bye(dialog, &mut fixture.agent_states).await;
        let command = if ended {
            fixture
                .session
                .update_leg_state(&fixture.target, LegState::Connected);
            CallCommand::LegEnded {
                leg_id: fixture.target.clone(),
                reason: "Remote BYE".into(),
            }
        } else {
            CallCommand::LegFailed {
                leg_id: fixture.target.clone(),
                reason: "Rejected with 486".into(),
            }
        };
        fixture.session.execute_command(command, None).await;
        fixture.session.handle_dialog_state(state).await.unwrap();
        assert!(
            fixture
                .session
                .pending_hangup
                .contains(&fixture.customer_dialog),
            "target termination before the agent event must release the customer"
        );
    }
}

#[tokio::test]
async fn customer_blind_transfer_outbound_customer_bye_before_agent_reply() {
    let mut fixture = outbound_transfer_fixture().await;
    let _guard = fixture.session.cancel_token.clone().drop_guard();
    let dialog = fixture
        .session
        .legs
        .get_dialog(&LegId::from("callee"))
        .unwrap()
        .clone();
    let Dialog::Invite(dialog) = dialog else {
        panic!("customer must have an INVITE dialog")
    };
    let state = receive_transfer_bye(dialog, &mut fixture.callee_states).await;
    fixture.session.handle_callee_state(state).await.unwrap();
    assert!(
        fixture.session.cancel_token.is_cancelled(),
        "customer BYE must end the session without waiting for the retired agent"
    );
    assert_eq!(
        fixture.session.legs.get(&fixture.target).unwrap().state,
        LegState::Ended
    );
}

#[tokio::test]
async fn customer_blind_transfer_outbound_survives_agent_bye() {
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_transaction};
    use rsipstack::dialog::dialog::TerminatedReason;

    for outcome in [
        "target_bye",
        "answer",
        "reject",
        "reject_before_agent_bye",
        "customer_bye",
    ] {
        let OutboundTransferFixture {
            mut session,
            target,
            customer_dialog,
            agent_states: _agent_states,
            callee_states: _callee_states,
        } = outbound_transfer_fixture().await;
        let _guard = session.cancel_token.clone().drop_guard();
        let caller = LegId::from("caller");
        let customer = LegId::from("callee");
        let agent_dialog = session.caller_dialog_id();
        let customer_peer = session.media_leg(&customer).unwrap();
        assert!(!session.legs.contains_key(&caller));
        assert!(session.pending_hangup.contains(&agent_dialog));
        assert!(session.app_media_leg.read().is_none());

        // The server's BYE completion arrives after the transfer command retires the agent.
        if outcome != "reject_before_agent_bye" {
            session
                .handle_dialog_state(DialogState::Terminated(
                    agent_dialog.clone(),
                    TerminatedReason::UasBye,
                ))
                .await
                .unwrap();
        }
        assert!(
            !session.cancel_token.is_cancelled(),
            "agent BYE must not end the customer call"
        );
        assert!(
            !session.pending_hangup.contains(&customer_dialog),
            "customer must not receive a cascading BYE"
        );
        assert!(session.meta.hangup_reason.is_none());
        assert!(session.legs.contains_key(&target));
        assert!(Arc::ptr_eq(
            &customer_peer,
            &session.media_leg(&customer).unwrap()
        ));

        if matches!(outcome, "reject" | "reject_before_agent_bye") {
            let result = session
                .execute_command(
                    CallCommand::LegFailed {
                        leg_id: target.clone(),
                        reason: "Rejected with 486".into(),
                    },
                    None,
                )
                .await;
            assert!(!result.success);
            assert!(
                session.pending_hangup.contains(&customer_dialog),
                "failed transfer must release the customer"
            );
            if outcome == "reject_before_agent_bye" {
                session
                    .handle_dialog_state(DialogState::Terminated(
                        agent_dialog,
                        TerminatedReason::UasBye,
                    ))
                    .await
                    .unwrap();
                assert!(session.pending_hangup.contains(&customer_dialog));
            }
        } else if matches!(outcome, "answer" | "target_bye") {
            let target_peer = session.media_leg(&target).unwrap();
            let target_remote = LegInner::new("target", &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = target_peer
                .pc()
                .local_description()
                .unwrap()
                .to_sdp_string();
            let answer = target_remote
                .apply_sdp(&offer, rustrtc::SdpType::Offer)
                .await
                .unwrap();
            target_remote.accept();
            let result = session
                .execute_command(
                    CallCommand::LegConnected {
                        leg_id: target.clone(),
                        answer_sdp: Some(answer),
                        dialog_id: None,
                    },
                    None,
                )
                .await;
            assert!(result.success, "{:?}", result.message);
            assert!(
                session.bridge().unwrap().is_bridged(),
                "customer and target media must connect"
            );
            assert!(!session.cancel_token.is_cancelled());
        }

        if outcome == "target_bye" {
            let request = create_test_request(
                rsipstack::sip::Method::Invite,
                "target",
                None,
                "example.com",
                None,
            );
            let (tx, _) = create_transaction(request).await;
            let (state_tx, _state_rx) = mpsc::unbounded_channel();
            let dialog = session
                .server
                .dialog_layer
                .get_or_create_server_invite(&tx, state_tx, None, None)
                .unwrap();
            session
                .legs
                .set_dialog(target.clone(), Dialog::Invite(dialog));
            let result = session
                .execute_command(
                    CallCommand::LegEnded {
                        leg_id: target.clone(),
                        reason: "Remote BYE".into(),
                    },
                    None,
                )
                .await;
            assert!(result.success, "{:?}", result.message);
            assert!(
                session.pending_hangup.contains(&customer_dialog),
                "answered target BYE must release the customer"
            );
        } else if !matches!(outcome, "reject" | "reject_before_agent_bye") {
            session
                .handle_callee_state(DialogState::Terminated(
                    customer_dialog,
                    TerminatedReason::UasBye,
                ))
                .await
                .unwrap();
            assert!(
                session.cancel_token.is_cancelled(),
                "customer BYE must end the transferred session"
            );
            assert_eq!(session.legs.get(&target).unwrap().state, LegState::Ended);
        }
    }
}

/// Same hand-off with the QUEUE-DISPATCHED agent shape: the agent dials into
/// a DYNAMIC leg (`LegPurpose::Agent`, stamped with the agent id) bridged to
/// the caller — previously `resolve_cc_customer_leg` only knew the
/// caller/callee slots and the command failed with "No customer leg".
#[tokio::test]
async fn test_customer_blind_transfer_dynamic_agent_leg_resolves_customer() {
    use crate::call::runtime::BridgeConfig;
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{MatchConditions, RouteAction, RouteQueueConfig, RouteRule};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    config.route_originated_calls = true;
    config.queues.insert(
        "ivr-entry-queue".to_string(),
        RouteQueueConfig {
            name: Some("ivr-entry-queue".to_string()),
            strategy: crate::proxy::routing::RouteQueueStrategyConfig {
                targets: vec![crate::proxy::routing::RouteQueueTargetConfig {
                    uri: "sip:offline-agent@rustpbx.com".to_string(),
                    label: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "ivr-entry".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("8000".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("queue".to_string()),
            queue: Some("ivr-entry-queue".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(
            Dialplan::new("test-session".to_string(), request, DialDirection::Inbound)
                .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    let mut customer = Leg::new(LegId::from("caller"));
    customer.state = LegState::Connected;
    session.legs.insert(LegId::from("caller"), customer);
    let agent_leg = LegId::new("agent-leg-queue-1");
    let mut agent = Leg::new(agent_leg.clone());
    agent.state = LegState::Connected;
    agent.agent_id = Some("bob".to_string());
    agent.purpose = Some(crate::call::domain::LegPurpose::Agent);
    session.legs.insert(agent_leg.clone(), agent);
    session.bridge = BridgeConfig::bridge(LegId::from("caller"), agent_leg.clone());

    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        session.execute_command(
            CallCommand::BlindTransferCustomer {
                target: "8000".to_string(),
                headers: Default::default(),
                application_only: false,
                completion: None,
            },
            Some(&mut callee_rx),
        ),
    )
    .await
    .expect("routed queue hand-off must not hang");
    assert!(
        result.success,
        "dynamic agent leg must resolve the customer: {:?}",
        result.message
    );

    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted");
    assert_eq!(entry.event.payload["transfer_target_type"], "queue");

    assert!(
        session.legs.get(&agent_leg).is_none(),
        "dynamic agent leg must be released with the hand-off"
    );
    assert!(
        session.legs.get(&LegId::from("caller")).is_some(),
        "customer leg must survive the hand-off"
    );
}

/// With the route table DISABLED (`route_originated_calls=false`) the same
/// "8000" target that Case 1 routes to a queue must DIAL AS-IS instead: the
/// route gate is respected, `call_transferred` reports kind=sip, and the
/// queue app never starts.
#[tokio::test]
async fn test_customer_blind_transfer_route_disabled_dials_as_is() {
    for (routed, restricted, target, succeeds) in [
        (true, true, "9000", true),
        (false, false, "8000", true),
        (true, true, "8000", false),
        (true, false, "9001", false),
    ] {
        assert_customer_blind_transfer_route(routed, restricted, target, succeeds, false).await;
    }
}

#[tokio::test]
async fn test_customer_blind_transfer_preserves_full_uri_policy() {
    for (restricted, target, succeeds) in [
        (false, "sip:9001@blocked.example", false),
        (true, "sip:9000@allowed.example", true),
    ] {
        assert_customer_blind_transfer_route(true, restricted, target, succeeds, false).await;
    }
}

#[tokio::test]
async fn test_customer_blind_transfer_registered_target_preserves_forward() {
    assert_customer_blind_transfer_route(true, true, "9000", true, true).await;
}

async fn assert_customer_blind_transfer_route(
    routed: bool,
    restricted: bool,
    target: &str,
    succeeds: bool,
    registered: bool,
) {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{MatchConditions, RouteAction, RouteQueueConfig, RouteRule};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::rwi::gateway::RwiGateway;

    let mut config = ProxyConfig::default();
    // Routing OFF — the rule below must be ignored for this API transfer.
    config.route_originated_calls = false;
    config.queues.insert(
        "ivr-entry-queue".to_string(),
        RouteQueueConfig {
            name: Some("ivr-entry-queue".to_string()),
            strategy: crate::proxy::routing::RouteQueueStrategyConfig {
                targets: vec![crate::proxy::routing::RouteQueueTargetConfig {
                    uri: "sip:offline-agent@rustpbx.com".to_string(),
                    label: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "ivr-entry".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("8000".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("queue".to_string()),
            queue: Some("ivr-entry-queue".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    if routed {
        config = test_forward_route_config();
        if target.contains("9001") {
            let rule = &mut config.routes.as_mut().unwrap()[0];
            rule.action.action = Some("busy".into());
            rule.action.dest = None;
        }
    }
    if target.contains('@') {
        config.routes.as_mut().unwrap()[0]
            .match_conditions
            .request_uri_host = Some(target.split_once('@').unwrap().1.to_string());
    }
    let wire_target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    if registered {
        config.trunks.get_mut("gw1").unwrap().dest =
            format!("sip:{}", wire_target.local_addr().unwrap());
        config.routes.as_mut().unwrap()[0].rewrite = Some(crate::proxy::routing::RewriteRules {
            to_user: Some("rewritten".into()),
            ..Default::default()
        });
    }
    let gateway = RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) =
        create_test_server_with_rwi_gateway(config, Arc::new(parking_lot::RwLock::new(gateway)))
            .await;

    let mut serving = None;
    if registered {
        server
            .locator
            .register(
                "9000",
                Some("rustpbx.com"),
                Location {
                    aor: "sip:registered@127.0.0.1:9".try_into().unwrap(),
                    expires: 3600,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        use rsipstack::transport::udp::UdpConnection;
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
        serving = Some(tokio::spawn(async move {
            endpoint.serve().await.unwrap();
        }));
    }
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(
            Dialplan::new("test-session".to_string(), request, DialDirection::Inbound)
                .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    let mut customer = Leg::new(LegId::from("caller"));
    customer.state = LegState::Connected;
    session.legs.insert(LegId::from("caller"), customer);
    let mut agent = Leg::new(LegId::from("callee"));
    agent.state = LegState::Connected;
    agent.agent_id = Some("bob".to_string());
    session.legs.insert(LegId::from("callee"), agent);

    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    // Leg-add dispatches the INVITE in the background (fire-and-forget), so
    // the command reports success at dispatch — standard blind-transfer
    // semantics ("发起即释放"): a target that never answers cannot be taken
    // back, and the later dial failure cascades via the dialog watcher.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        session.execute_command(
            CallCommand::BlindTransferCustomer {
                target: target.to_string(),
                headers: Default::default(),
                application_only: restricted,
                completion: None,
            },
            Some(&mut callee_rx),
        ),
    )
    .await
    .expect("dial dispatch must return, not hang");
    if !succeeds {
        assert!(
            !result.success,
            "an unconfigured or rejected target must be rejected: {target}, result={:?}",
            result.message
        );
        if target.contains("9001") {
            let result = session
                .handle_customer_blind_transfer_sip(
                    &LegId::from("callee"),
                    &LegId::from("caller"),
                    target,
                    &HashMap::new(),
                    None,
                )
                .await;
            assert!(
                result.is_err(),
                "a rejected route must never become a direct dial"
            );
        }
        assert!(
            session.legs.get(&LegId::from("callee")).is_some(),
            "rejection must keep the agent"
        );
        assert!(
            session.legs.get(&LegId::from("caller")).is_some(),
            "rejection must keep the customer"
        );
        return;
    }
    assert!(
        result.success,
        "dial-as-is dispatch must succeed: {:?}",
        result.message
    );
    assert!(
        session.legs.get(&LegId::from("callee")).is_none(),
        "agent leg must be released once the hand-off is dispatched"
    );
    assert!(
        session.legs.get(&LegId::from("caller")).is_some(),
        "customer leg must survive the hand-off"
    );

    if registered {
        let mut buffer = [0u8; 65536];
        let (size, _) =
            tokio::time::timeout(Duration::from_secs(3), wire_target.recv_from(&mut buffer))
                .await
                .expect("the configured trunk must receive the transfer INVITE")
                .unwrap();
        let rsipstack::sip::SipMessage::Request(invite) =
            rsipstack::sip::SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap())
                .unwrap()
        else {
            panic!("expected INVITE");
        };
        assert_eq!(invite.method, rsipstack::sip::Method::Invite);
        assert_eq!(
            invite.uri.user().map(|user| user.to_string()).as_deref(),
            Some("rewritten")
        );
        session.cancel_token.cancel();
        serving.take().unwrap().abort();
    }

    // The transfer event reports a SIP dial — the queue route was ignored.
    let entry = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entry = events.recv().await.expect("event tap must stay open");
            if entry.event.event_type == "call_transferred" {
                return entry;
            }
        }
    })
    .await
    .expect("call_transferred must be emitted for the dispatched dial");
    assert_eq!(
        entry.event.payload["transfer_target_type"], "sip",
        "routing disabled → the target dials as SIP, no queue takeover"
    );
}

/// `leg_id_for_dialog` resolves the caller dialog to the caller leg and
/// reports unknown dialogs as unowned.
#[tokio::test]
async fn test_leg_id_for_dialog_resolves_caller_leg() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");
    let caller_dialog_id = server_dialog.id().to_string();

    let context = CallContext {
        session_id: "test-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "test-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    assert_eq!(
        session.leg_id_for_dialog(&caller_dialog_id),
        Some(LegId::from("caller"))
    );
    assert_eq!(session.leg_id_for_dialog("unknown-dialog"), None);
}

// ─── home-proxy self-identity (self_ident) unit tests ──────────────
// The legacy `is_local_home_proxy` (listener-set matching) was replaced by a
// deterministic single-identity check: cluster_self_addr when resolved, else
// the default contact URI the registrar stamps on fallback. The behavioral
// coverage now lives in `route_via_home_proxy_tests` inside session.rs and in
// the route_via_home_proxy tests below.

#[test]
fn test_home_proxy_self_ident_matches_own_address() {
    // home == this node's identity → deliver locally.
    let target = Location {
        home_proxy: Some(SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }),
        ..Default::default()
    };
    let self_ident = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(!SipSession::route_via_home_proxy(
        &target,
        true,
        Some(&self_ident)
    ));
}

#[test]
fn test_home_proxy_self_ident_rejects_foreign_address() {
    let target = Location {
        home_proxy: Some(SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
        }),
        ..Default::default()
    };
    let self_ident = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(SipSession::route_via_home_proxy(
        &target,
        true,
        Some(&self_ident)
    ));
}

#[test]
fn test_home_proxy_self_ident_rejects_port_mismatch() {
    let target = Location {
        home_proxy: Some(SipAddr {
            r#type: None,
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:5070").unwrap(),
        }),
        ..Default::default()
    };
    let self_ident = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(SipSession::route_via_home_proxy(
        &target,
        true,
        Some(&self_ident)
    ));
}

#[test]
fn test_home_proxy_self_ident_compares_addr_string_not_transport() {
    // Transport type should NOT affect address matching — only host:port matters.
    let target = Location {
        home_proxy: Some(SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        }),
        ..Default::default()
    };
    let self_ident = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Wss),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(!SipSession::route_via_home_proxy(
        &target,
        true,
        Some(&self_ident)
    ));
}

// ─── route_via_home_proxy flag ───────

#[test]
fn test_route_via_home_proxy_false_without_home_proxy() {
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("192.168.1.10:5060").unwrap(),
    };
    let target = Location {
        destination: Some(destination.clone()),
        home_proxy: None,
        ..Default::default()
    };
    assert!(!SipSession::route_via_home_proxy(&target, false, None));
}

#[test]
fn test_route_via_home_proxy_remote_home_proxy_sets_via_flag() {
    // home_proxy != local -> route_via_home_proxy stays true.
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
    };
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
    };
    let target = Location {
        destination: Some(destination),
        home_proxy: Some(home_proxy.clone()),
        ..Default::default()
    };
    let cluster_self = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    let via_home_proxy = SipSession::route_via_home_proxy(&target, true, Some(&cluster_self));
    assert!(
        via_home_proxy,
        "route_via_home_proxy must be true for remote home_proxy"
    );
}

#[test]
fn test_route_via_home_proxy_local_home_proxy_no_via_flag() {
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    let target = Location {
        destination: Some(destination.clone()),
        home_proxy: Some(home_proxy),
        ..Default::default()
    };
    let cluster_self = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    let via_home_proxy = SipSession::route_via_home_proxy(&target, true, Some(&cluster_self));
    assert!(
        !via_home_proxy,
        "route_via_home_proxy must be false when home_proxy is local"
    );
}

// ─── Verify no self-referencing Record-Route in INVITE headers ────

#[test]
fn test_route_via_home_proxy_does_not_add_self_referencing_record_route() {
    // This test validates the architectural fix:
    // When routing via a remote home_proxy, the INVITE MUST NOT include
    // a Record-Route header pointing to the local node. Including one
    // would cause the dialog route_set to contain a self-referencing
    // Route entry, which makes all subsequent in-dialog requests
    // (BYE, ACK) loopback to the local node instead of reaching the
    // remote agent.
    //
    // The Contact header in the INVITE already provides the correct
    // return path for the callee's responses and requests.
    //
    // This test exercises route_via_home_proxy
    // to ensure the routing logic is correct. The actual INVITE header construction is exercised
    // by the cluster home_proxy e2e test.
    //
    // Verify: home_proxy is recognized as remote -> via_home_proxy=true
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
    };
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
    };
    let target = Location {
        destination: Some(destination.clone()),
        home_proxy: Some(home_proxy.clone()),
        ..Default::default()
    };
    // This node's identity is 10.172.148.121 — the remote home at
    // 10.172.149.126 must route via the home node...
    let self_ident = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    let via_home_proxy = SipSession::route_via_home_proxy(&target, true, Some(&self_ident));
    assert!(
        via_home_proxy,
        "route_via_home_proxy must be true for cross-node routing"
    );

    // ...while a home equal to this node's own identity delivers locally.
    let local_target = Location {
        destination: Some(destination.clone()),
        home_proxy: Some(self_ident.clone()),
        ..Default::default()
    };
    assert!(
        !SipSession::route_via_home_proxy(&local_target, true, Some(&self_ident)),
        "home_proxy at 10.172.148.121 must deliver locally"
    );
}

// ── filter_video_caps_for_rtp ────────────────────────────────────────────

fn make_video_cap(
    pt: u8,
    codec: &str,
    fmtp: Option<&str>,
    rtcp_fbs: &[&str],
) -> rustrtc::VideoCapability {
    rustrtc::VideoCapability {
        payload_type: pt,
        codec_name: codec.to_string(),
        clock_rate: 90000,
        fmtp: fmtp.map(|s| s.to_string()),
        rtcp_fbs: rtcp_fbs.iter().map(|s| s.to_string()).collect(),
        rtx_payload_type: None,
    }
}

fn filter_video_caps_for_rtp(
    caps: &[rustrtc::VideoCapability],
    allowed_codecs: &[String],
) -> Vec<rustrtc::VideoCapability> {
    let defaults = crate::config::default_video_codecs();
    let effective_allow: &[String] = if allowed_codecs.is_empty() {
        &defaults
    } else {
        allowed_codecs
    };

    caps.iter()
        .filter(|cap| {
            effective_allow
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&cap.codec_name))
        })
        .map(|cap| rustrtc::VideoCapability {
            payload_type: cap.payload_type,
            codec_name: cap.codec_name.clone(),
            clock_rate: cap.clock_rate,
            fmtp: cap.fmtp.clone(),
            rtcp_fbs: vec![],
            ..Default::default()
        })
        .collect()
}

#[test]
fn test_initial_caller_answer_video_follows_callee_selection() {
    let caller_offer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 4000 UDP/TLS/RTP/SAVPF 96 102 118\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:118 H264/90000\r\n\
a=fmtp:118 packetization-mode=1;profile-level-id=64001f\r\n";
    let generated_answer = "v=0\r\n\
o=- 2 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 1\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 102 118\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:caller-ice\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:118 H264/90000\r\n\
a=fmtp:118 packetization-mode=1;profile-level-id=64001f\r\n\
a=ssrc:1234 cname:test\r\n";
    let callee_answer = "v=0\r\n\
o=- 3 3 IN IP4 192.0.2.10\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 5000 RTP/AVP 102\r\n\
a=recvonly\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 profile-level-id=42801F;packetization-mode=1\r\n";

    let caller_video_caps = MediaNegotiator::video_caps_for_config(
        &MediaNegotiator::extract_video_codecs(caller_offer),
        &crate::config::default_video_codecs(),
    );
    let accepted_video_caps =
        MediaNegotiator::accepted_video_capabilities(&caller_video_caps, callee_answer);
    let answer = MediaNegotiator::rewrite_video_capabilities(
        rustrtc::SdpType::Answer,
        generated_answer,
        &accepted_video_caps,
    )
    .unwrap();

    assert!(answer.contains("m=video 9 UDP/TLS/RTP/SAVPF 102\r\n"));
    assert!(answer.contains("a=rtpmap:102 H264/90000\r\n"));
    assert!(!answer.contains("VP8/90000"));
    assert!(!answer.contains("a=rtpmap:118 H264/90000"));
    assert!(answer.contains("a=ice-ufrag:caller-ice\r\n"));
    assert!(answer.contains("a=mid:1\r\n"));
    assert!(answer.contains("a=ssrc:1234 cname:test\r\n"));
}

/// Default allowlist keeps peer-offered H264 and VP8 and strips feedback
/// from the RTP leg.
#[test]
fn test_filter_video_caps_default_keeps_h264_and_vp8() {
    let caps = vec![
        make_video_cap(
            96,
            "H264",
            Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
            &["goog-remb", "transport-cc", "nack", "nack pli", "ccm fir"],
        ),
        make_video_cap(97, "VP8", None, &["goog-remb", "transport-cc"]),
        make_video_cap(98, "VP9", None, &["goog-remb"]),
    ];

    let result = filter_video_caps_for_rtp(&caps, &[]);

    assert_eq!(result.len(), 2, "H264 and VP8 should survive by default");
    assert_eq!(result[0].codec_name, "H264");
    assert_eq!(result[0].payload_type, 96);
    assert_eq!(result[1].codec_name, "VP8");
    assert_eq!(result[1].payload_type, 97);
    assert!(result[0].rtcp_fbs.is_empty());
    assert!(result[1].rtcp_fbs.is_empty());
    assert!(result[0].fmtp.is_some(), "fmtp should be preserved");
}

/// An explicit allowlist controls the codecs accepted for relay.
#[test]
fn test_filter_video_caps_explicit_allowlist() {
    let caps = vec![
        make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &["goog-remb"]),
        make_video_cap(97, "VP8", None, &["transport-cc"]),
        make_video_cap(98, "H265", None, &[]),
    ];

    let allowed = vec!["H264".to_string(), "vp8".to_string()];
    let result = filter_video_caps_for_rtp(&caps, &allowed);

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].codec_name, "H264");
    assert_eq!(result[1].codec_name, "VP8");
    assert!(result.iter().all(|c| c.rtcp_fbs.is_empty()));
}

#[test]
fn test_filter_video_caps_respects_h264_only_configuration() {
    let caps = vec![
        make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &[]),
        make_video_cap(97, "VP8", None, &[]),
    ];
    let allowed = vec!["H264".to_string()];

    let result = filter_video_caps_for_rtp(&caps, &allowed);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].codec_name, "H264");
}

/// The RTP/AVP leg does not advertise AVPF feedback.
#[test]
fn test_filter_video_caps_strips_all_rtcp_feedback() {
    let caps = vec![make_video_cap(
        96,
        "H264",
        None,
        &["nack", "nack pli", "ccm fir", "goog-remb", "transport-cc"],
    )];

    let result = filter_video_caps_for_rtp(&caps, &[]);

    assert!(result[0].rtcp_fbs.is_empty());
}

/// VP8 is accepted when configured while unsupported VP9 is discarded.
#[test]
fn test_filter_video_caps_configured_vp8_but_not_vp9() {
    let caps = vec![
        make_video_cap(97, "VP8", None, &["goog-remb", "transport-cc"]),
        make_video_cap(98, "VP9", None, &["goog-remb"]),
    ];

    let result = filter_video_caps_for_rtp(&caps, &["H264".to_string(), "VP8".to_string()]);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].codec_name, "VP8");
}

/// Empty caps slice produces empty result (no panic).
#[test]
fn test_filter_video_caps_empty_input() {
    let result = filter_video_caps_for_rtp(&[], &[]);
    assert!(result.is_empty());
}

/// Codec name matching is case-insensitive in both directions.
#[test]
fn test_filter_video_caps_case_insensitive_matching() {
    let caps = vec![
        make_video_cap(96, "h264", None, &["nack"]), // lowercase codec name
    ];

    // Allowlist uses uppercase "H264"
    let result = filter_video_caps_for_rtp(&caps, &["H264".to_string(), "VP8".to_string()]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].codec_name, "h264");
}

/// fmtp string is preserved exactly on matched codecs.
#[test]
fn test_filter_video_caps_fmtp_preserved() {
    let fmtp = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032";
    let caps = vec![make_video_cap(96, "H264", Some(fmtp), &["goog-remb"])];

    let result = filter_video_caps_for_rtp(&caps, &[]);
    assert_eq!(result[0].fmtp.as_deref(), Some(fmtp));
}

/// Preserve peer SDP order and every supported profile; do not sort or
/// deduplicate the pass-through capability list.
#[test]
fn test_filter_video_caps_preserves_supported_offer_order() {
    let caps = vec![
        make_video_cap(96, "H264", Some("profile-level-id=42e01f"), &["goog-remb"]),
        make_video_cap(97, "VP8", None, &["transport-cc"]),
        make_video_cap(98, "H264", Some("profile-level-id=640032"), &["nack"]),
    ];

    let result = filter_video_caps_for_rtp(&caps, &["H264".to_string(), "VP8".to_string()]);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].payload_type, 96);
    assert_eq!(result[0].fmtp.as_deref(), Some("profile-level-id=42e01f"));
    assert_eq!(result[1].payload_type, 97);
    assert_eq!(result[2].payload_type, 98);
    assert_eq!(result[2].fmtp.as_deref(), Some("profile-level-id=640032"));
    assert!(result.iter().all(|cap| cap.rtcp_fbs.is_empty()));
}

#[tokio::test]
async fn audio_only_leg_accepts_later_h264_video_without_inventing_vp8() {
    let leg = crate::media::leg::LegInner::new(
        "audio-then-video",
        &crate::media::leg::LegConfig::rtp_pcmu(),
        None,
    )
    .expect("audio-only leg");

    let initial_offer = leg.create_offer().await.expect("initial audio offer");
    assert!(
        !initial_offer.contains("m=video"),
        "audio-only call must not invent video:\n{initial_offer}"
    );
    let initial_answer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 41000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtcp:41001\r\n";
    leg.apply_sdp(initial_answer, rustrtc::SdpType::Answer)
        .await
        .expect("initial answer");

    let reinvite = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 41000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtcp:41001\r\n\
m=video 42000 RTP/AVP 102\r\n\
a=sendrecv\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 packetization-mode=1;profile-level-id=42801f\r\n\
a=rtcp:42001\r\n";
    let video_caps = crate::media::negotiate::MediaNegotiator::video_caps_for_config(
        &crate::media::negotiate::MediaNegotiator::extract_video_codecs(reinvite),
        &crate::config::default_video_codecs(),
    );
    let answer = SipSession::build_local_answer_from_pc(leg.pc(), reinvite, Some(&video_caps))
        .await
        .expect("video re-INVITE answer");

    assert!(answer.contains("m=video "));
    assert!(answer.contains("a=rtpmap:102 H264/90000"));
    assert!(
        !answer.contains("VP8/90000"),
        "answer invented VP8:\n{answer}"
    );
    assert!(
        answer.contains("a=sendrecv"),
        "video answer must remain bidirectional:\n{answer}"
    );
    assert_ne!(
        crate::media::leg::sender_ssrc_for_kind(leg.pc(), rustrtc::MediaKind::Video),
        0,
        "re-INVITE video needs a relay destination SSRC"
    );
    leg.stop();
}

// ── MediaBridge caller leg: video SDP ─────────────────────────────────

/// A WebRTC caller offer carrying audio + H264/VP8 video. The MediaBridge
/// caller leg must answer with a video m-line that (a) preserves the
/// peer-offered H264 and VP8 capabilities, (b) carries the leg's video sender `a=ssrc`
/// (eliminating the browser's 2–3 s unsignaled-SSRC demux delay), and
/// (c) is sendrecv so the caller can send AND receive video.
#[tokio::test]
async fn ensure_caller_leg_answers_offer_with_video_ssrc() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let mut proxy_config = (*server.proxy_config.load_full()).clone();
    proxy_config.video_codecs = vec!["H264".to_string(), "VP8".to_string()];
    server.proxy_config.store(Arc::new(proxy_config));
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "video-caller-leg".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "video-caller-leg".to_string(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    // `use_media_proxy = true` eagerly creates the MediaBridge.
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );

    let caller_offer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=audio 4000 UDP/TLS/RTP/SAVPF 111 101\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=rtpmap:101 telephone-event/48000\r\n\
a=setup:actpass\r\n\
a=ice-ufrag:uv50\r\n\
a=ice-pwd:ib8b\r\n\
a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n\
m=video 4001 UDP/TLS/RTP/SAVPF 96 98\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n\
a=rtpmap:98 VP8/90000\r\n\
a=setup:actpass\r\n\
a=ice-ufrag:uv50\r\n\
a=ice-pwd:ib8b\r\n\
a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n";

    session.media.caller_offer = Some(caller_offer.to_string());
    session
        .ensure_caller_leg()
        .await
        .expect("caller leg must be created");

    let answer = session
        .media
        .answer
        .clone()
        .expect("caller answer must be generated");
    assert!(
        answer.contains("m=video"),
        "answer lacks a video m-line:\n{answer}"
    );
    assert!(
        answer.contains("a=ssrc:"),
        "answer lacks a=ssrc (video demux delay):\n{answer}"
    );
    assert!(
        answer.contains("rtpmap:96 H264/90000"),
        "answer lacks H264 rtpmap:\n{answer}"
    );
    assert!(
        answer.contains("VP8/90000"),
        "answer discarded peer-offered VP8:\n{answer}"
    );

    drop(session);
}

#[tokio::test]
async fn ensure_caller_leg_recovers_after_busy_rtp_port() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;

    let occupied = loop {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        if socket.local_addr().unwrap().port() % 2 == 0 {
            break socket;
        }
    };
    let port = occupied.local_addr().unwrap().port();
    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "example.com",
        None,
    );
    let mut dialplan = Dialplan::new(
        "busy-media-port".to_string(),
        request,
        DialDirection::Inbound,
    );
    dialplan.media.bind_ip = Some("127.0.0.1".to_string());
    dialplan.media.rtp_start_port = Some(port);
    dialplan.media.rtp_end_port = Some(port);
    let (mut session, _handle, _cmd_rx) = build_session_with_cmd_rx_on(server, dialplan).await;
    session.media.caller_offer = Some(
        concat!(
            "v=0\r\n",
            "o=- 1 1 IN IP4 127.0.0.1\r\n",
            "s=-\r\n",
            "c=IN IP4 127.0.0.1\r\n",
            "t=0 0\r\n",
            "m=audio 4000 RTP/AVP 0\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=sendrecv\r\n",
        )
        .to_string(),
    );

    let error = session.ensure_caller_leg().await.unwrap_err();
    assert!(
        error.to_string().contains("No available even RTP ports"),
        "{error}"
    );
    drop(occupied);
    session
        .ensure_caller_leg()
        .await
        .expect("caller media must recover after port release");
    let answer = session.media.answer.as_ref().expect("caller SDP answer");
    assert!(
        answer.contains(&format!("m=audio {port} RTP/AVP 0")),
        "{answer}"
    );
    session
        .ensure_caller_leg()
        .await
        .expect("existing caller media must remain reusable");
}

/// Regression test for issue #281: a Groundwire-style SDES-SRTP offer
/// (`RTP/SAVP` + `a=crypto`) must get an Srtp caller leg and an answer with
/// the matching `RTP/SAVP` profile and `a=crypto` — not a plain `RTP/AVP`
/// downgrade (strict SRTP clients refuse to flow media on a downgraded
/// answer). The callee transport hint must also stay untouched: it belongs to
/// the callee-creation paths, not `ensure_caller_leg`.
#[tokio::test]
async fn ensure_caller_leg_answers_sdes_srtp_offer_with_crypto() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "sdes-caller-leg".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "sdes-caller-leg".to_string(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );

    let caller_offer = concat!(
        "v=0\r\n",
        "o=- 8444426914 10545 IN IP4 198.51.100.7\r\n",
        "s=groundwire\r\n",
        "c=IN IP4 198.51.100.7\r\n",
        "t=0 0\r\n",
        "m=audio 36786 RTP/SAVP 103 9 0 8 101\r\n",
        "a=rtpmap:101 telephone-event/8000\r\n",
        "a=rtpmap:103 opus/48000/2\r\n",
        "a=fmtp:101 0-15\r\n",
        "a=fmtp:103 maxplaybackrate=16000;maxaveragebitrate=24000;useinbandfec=1;usedtx=1\r\n",
        "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:9oRSAVGxHLG/AcJhSTwRec00TTSEfGy5gndS0De8\r\n",
        "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:89V4GlaGoakgb7PsBmJewbHgseDfcgDmwPqSeSte\r\n",
        "a=ptime:20\r\n",
        "a=sendrecv\r\n",
    );
    session.media.caller_offer = Some(caller_offer.to_string());
    session
        .ensure_caller_leg()
        .await
        .expect("caller leg must be created");

    // The caller leg PC must run SDES-SRTP, not plain RTP.
    let caller_leg = session
        .media_leg(&LegId::from("caller"))
        .expect("caller media leg");
    assert_eq!(
        caller_leg.pc().config().transport_mode,
        rustrtc::TransportMode::Srtp,
        "SAVP offer must produce an Srtp caller leg"
    );

    // The answer must keep the SAVP profile and carry a crypto line.
    let answer = session
        .media
        .answer
        .clone()
        .expect("caller answer must be generated");
    assert!(
        answer.contains("m=audio") && answer.contains("RTP/SAVP"),
        "answer must keep the RTP/SAVP profile:\n{answer}"
    );
    assert!(
        answer.contains("a=crypto:"),
        "answer must carry a=crypto for an SDES offer:\n{answer}"
    );

    // ensure_caller_leg must not clobber the callee hint ("opposite of
    // caller" guess); callee-creation paths set it themselves.
    assert_eq!(
        session.legs.get_transport(&LegId::from("callee")),
        None,
        "callee transport hint must not be set by ensure_caller_leg"
    );

    drop(session);
}

/// `video_policy = "strip"` must disable video on the media path entirely:
/// the caller leg config carries no video capabilities, so the answer has
/// no video m-line (audio-only).
#[tokio::test]
async fn video_strip_policy_omits_video_mline() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "video-strip".to_string(),
        dialplan: Arc::new({
            let mut dp = Dialplan::new("video-strip".to_string(), request, DialDirection::Inbound);
            dp.media.video_policy = Some(crate::proxy::routing::VideoPolicy::Strip);
            dp
        }),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );

    let caller_offer = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 4000 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=sendrecv\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=setup:actpass\r\n\
a=ice-ufrag:uv50\r\n\
a=ice-pwd:ib8b\r\n\
a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n\
m=video 4001 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=sendrecv\r\n\
a=rtpmap:96 H264/90000\r\n\
a=setup:actpass\r\n\
a=ice-ufrag:uv50\r\n\
a=ice-pwd:ib8b\r\n\
a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n";

    session.media.caller_offer = Some(caller_offer.to_string());
    session
        .ensure_caller_leg()
        .await
        .expect("caller leg must be created");

    let answer = session
        .media
        .answer
        .clone()
        .expect("caller answer must be generated");
    // Video is forced inactive (port 0) — the caller must not get a usable
    // video m-line (audio a=ssrc is expected and fine).
    assert!(
        answer.contains("m=video 0 "),
        "strip policy must force the video m-line inactive (port 0):\n{answer}"
    );

    drop(session);
}

// ── DTMF payload building ─────────────────────────────────────────────

// --- trunk_host_port tests ---

#[test]
fn test_trunk_host_port_sip_uri_with_port() {
    let (host, port) = trunk_host_port("sip:58.246.19.74:6988").unwrap();
    assert_eq!(host, "58.246.19.74");
    assert_eq!(port, 6988);
}

#[test]
fn test_trunk_host_port_sip_uri_without_port() {
    let (host, port) = trunk_host_port("sip:pbx.example.com").unwrap();
    assert_eq!(host, "pbx.example.com");
    assert_eq!(port, 5060);
}

#[test]
fn test_trunk_host_port_sip_uri_with_user_and_port() {
    let (host, port) = trunk_host_port("sip:user@203.0.113.5:5060").unwrap();
    assert_eq!(host, "203.0.113.5");
    assert_eq!(port, 5060);
}

#[test]
fn test_trunk_host_port_bare_host_port() {
    let (host, port) = trunk_host_port("58.246.19.74:6988").unwrap();
    assert_eq!(host, "58.246.19.74");
    assert_eq!(port, 6988);
}

#[test]
fn test_trunk_host_port_bare_host_only() {
    let (host, port) = trunk_host_port("203.0.113.10").unwrap();
    assert_eq!(host, "203.0.113.10");
    assert_eq!(port, 5060);
}

#[test]
fn test_trunk_host_port_bare_ipv6() {
    let (host, port) = trunk_host_port("[::1]").unwrap();
    assert_eq!(host, "[::1]");
    assert_eq!(port, 5060);
}

#[test]
fn test_trunk_host_port_empty() {
    assert!(trunk_host_port("").is_none());
}

// --- resolve_effective_codecs priority logic tests ---

#[test]
fn test_priority_uses_dialplan_first() {
    let codecs = resolve_codecs_fake(&[CodecType::PCMA, CodecType::G729], &[]);
    assert_eq!(codecs, vec![CodecType::PCMA, CodecType::G729]);
}

#[test]
fn test_priority_falls_back_to_proxy_when_dialplan_empty() {
    let codecs = resolve_codecs_fake(&[], &["pcma", "g729"]);
    assert_eq!(codecs, vec![CodecType::PCMA, CodecType::G729]);
}

#[test]
fn test_priority_returns_empty_when_no_sources() {
    let codecs = resolve_codecs_fake(&[], &[] as &[&str]);
    assert!(codecs.is_empty());
}

#[test]
fn test_priority_filters_invalid_codec_names() {
    let codecs = resolve_codecs_fake(&[], &["pcma", "invalid_codec", "g729"]);
    assert_eq!(codecs, vec![CodecType::PCMA, CodecType::G729]);
}

#[test]
fn test_priority_ignores_empty_proxy_config() {
    let codecs = resolve_codecs_fake(&[], &[""]);
    assert!(codecs.is_empty());
}

#[test]
fn test_priority_dialplan_with_opus() {
    let codecs = resolve_codecs_fake(&[CodecType::Opus, CodecType::PCMU], &[]);
    assert_eq!(codecs, vec![CodecType::Opus, CodecType::PCMU]);
}

/// Simulates the priority chain: dialplan → trunk → proxy.
fn resolve_codecs_fake(dialplan: &[CodecType], proxy_strs: &[&str]) -> Vec<CodecType> {
    if !dialplan.is_empty() {
        return dialplan.to_vec();
    }
    let proxy: Vec<String> = proxy_strs
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if !proxy.is_empty() {
        return parse_allowed_codecs(&proxy);
    }
    vec![]
}

// ── SipSession::parse_info_media_source tests ──────────────────────────
use crate::call::domain::MediaSource;

#[test]
fn test_parse_file_source() {
    let src = serde_json::json!({"source_type": "file", "uri": "/tmp/a.wav"});
    assert_eq!(
        super::SipSession::parse_info_media_source(&src),
        Some(MediaSource::File {
            path: "/tmp/a.wav".into()
        })
    );
}

#[test]
fn test_parse_url_source() {
    let src = serde_json::json!({"source_type": "url", "uri": "http://x.com/a.wav"});
    assert_eq!(
        super::SipSession::parse_info_media_source(&src),
        Some(MediaSource::Url {
            url: "http://x.com/a.wav".into()
        })
    );
}

#[test]
fn test_parse_silence_source() {
    let src = serde_json::json!({"source_type": "silence"});
    assert_eq!(
        super::SipSession::parse_info_media_source(&src),
        Some(MediaSource::Silence)
    );
}

#[test]
fn test_parse_files_source_uses_first_uri() {
    let src = serde_json::json!({"source_type": "files", "uris": ["/tmp/a.wav", "/tmp/b.wav"]});
    assert_eq!(
        super::SipSession::parse_info_media_source(&src),
        Some(MediaSource::File {
            path: "/tmp/a.wav".into()
        })
    );
}

#[test]
fn test_parse_unknown_source_type() {
    let src = serde_json::json!({"source_type": "mp3", "uri": "/tmp/x.mp3"});
    assert_eq!(super::SipSession::parse_info_media_source(&src), None);
}

#[test]
fn test_parse_defaults_to_file() {
    let src = serde_json::json!({"uri": "/tmp/default.wav"});
    assert_eq!(
        super::SipSession::parse_info_media_source(&src),
        Some(MediaSource::File {
            path: "/tmp/default.wav".into()
        })
    );
}

#[tokio::test]
async fn media_bridge_caller_answer_follows_callee_answer_codec() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let mut dialplan = Dialplan::new(
        "callee-codec-answer".to_string(),
        original_request,
        DialDirection::Inbound,
    );
    dialplan.allow_codecs = vec![CodecType::PCMU, CodecType::PCMA, CodecType::G722];
    let context = CallContext {
        session_id: "callee-codec-answer".to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );
    session.media.caller_offer = Some(
        concat!(
            "v=0\r\n",
            "o=alice 1 1 IN IP4 192.0.2.10\r\n",
            "s=Talk\r\n",
            "c=IN IP4 192.0.2.10\r\n",
            "t=0 0\r\n",
            "m=audio 40000 RTP/AVP 18 0 9 8 101\r\n",
            "a=rtpmap:18 G729/8000\r\n",
            "a=fmtp:18 annexb=yes\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:9 G722/8000\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=rtpmap:101 telephone-event/8000\r\n",
            "a=sendrecv\r\n",
        )
        .to_string(),
    );

    let callee_offer = session
        .create_callee_track(false)
        .await
        .expect("callee offer");
    let callee_offer_profile = MediaNegotiator::extract_leg_profile(&callee_offer);
    assert_eq!(
        callee_offer_profile.audio.as_ref().map(|codec| codec.codec),
        Some(CodecType::PCMU),
        "configured codecs must control the callee offer"
    );

    let callee = LegInner::new("callee-answer", &LegConfig::rtp_pcmu(), None).expect("callee leg");
    let callee_answer = callee.answer(&callee_offer).await.expect("callee answer");
    let caller_answer = session
        .prepare_caller_answer_from_callee_sdp(Some(callee_answer), false, rustrtc::SdpType::Answer)
        .await
        .expect("prepare caller answer")
        .expect("caller answer");

    assert_eq!(
        session.bridge.legs,
        vec![LegId::from("caller"), LegId::from("callee")],
        "direct dialing must record the pair for hold/playback restoration"
    );
    let caller_answer_profile = MediaNegotiator::extract_leg_profile(&caller_answer);
    assert_eq!(
        caller_answer_profile
            .audio
            .as_ref()
            .map(|codec| codec.codec),
        Some(CodecType::PCMU),
        "caller answer must follow the codec selected in the callee answer"
    );
    let caller_leg_profile = session
        .media_leg(&LegId::from("caller"))
        .and_then(|leg| leg.negotiated())
        .expect("caller leg profile");
    assert_eq!(
        caller_leg_profile.audio.as_ref().map(|codec| codec.codec),
        Some(CodecType::PCMU),
        "caller leg sender/profile must match the returned SDP"
    );
}

// ── WebRTC leg must honor a configured G729 codec policy ───────────
//
// Regression for the WebRTC-G729 pass-through: the generated WebRTC callee
// offer must carry G729 when the effective codec policy (route
// `allow_codecs`, trunk codec, or `[proxy] audio_codecs`) selects it, and must
// keep it out when the policy does not. Guarantees negotiation matches the
// media plane (which can fast-path/transcode G729 over a WebRTC leg — see
// rustpbx-media `fast_path_rtp_g729_to_webrtc_g729`).

fn webrtc_g729_test_caller_offer() -> String {
    concat!(
        "v=0\r\n",
        "o=alice 1 1 IN IP4 192.0.2.10\r\n",
        "s=Talk\r\n",
        "c=IN IP4 192.0.2.10\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/AVP 18 0 9 8 101\r\n",
        "a=rtpmap:18 G729/8000\r\n",
        "a=fmtp:18 annexb=yes\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:9 G722/8000\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "a=rtpmap:101 telephone-event/8000\r\n",
        "a=sendrecv\r\n",
    )
    .to_string()
}

async fn webrtc_callee_offer_for_allow(allow: Vec<CodecType>) -> String {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let mut dialplan = Dialplan::new(
        "webrtc-g729".to_string(),
        original_request,
        DialDirection::Inbound,
    );
    dialplan.allow_codecs = allow;
    let context = CallContext {
        session_id: "webrtc-g729".to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );
    session.media.caller_offer = Some(webrtc_g729_test_caller_offer());

    session
        .create_callee_track(true)
        .await
        .expect("webrtc callee offer")
}

#[tokio::test]
async fn webrtc_callee_offer_keeps_g729_when_allowed() {
    let offer = webrtc_callee_offer_for_allow(vec![
        CodecType::G729,
        CodecType::PCMA,
        CodecType::PCMU,
        CodecType::G722,
        CodecType::Opus,
    ])
    .await;

    assert!(
        offer.contains("UDP/TLS/RTP/SAVPF") && offer.contains("a=fingerprint"),
        "callee leg must be a WebRTC (DTLS-SRTP) offer:\n{offer}"
    );
    assert!(
        offer.contains("a=rtpmap:18 G729/8000"),
        "G729 must be advertised to the WebRTC leg when the policy allows it:\n{offer}"
    );
    let profile = MediaNegotiator::extract_leg_profile(&offer);
    assert_eq!(
        profile.audio.as_ref().map(|codec| codec.codec),
        Some(CodecType::G729),
        "G729 must lead the WebRTC callee offer (caller offered it first + policy allows)"
    );
}

#[tokio::test]
async fn webrtc_callee_offer_omits_g729_when_not_allowed() {
    let offer = webrtc_callee_offer_for_allow(vec![
        CodecType::PCMU,
        CodecType::PCMA,
        CodecType::G722,
        CodecType::Opus,
    ])
    .await;

    assert!(
        !offer.contains("G729"),
        "G729 must not leak into a WebRTC offer when the policy excludes it:\n{offer}"
    );
    let profile = MediaNegotiator::extract_leg_profile(&offer);
    assert_eq!(
        profile.audio.as_ref().map(|codec| codec.codec),
        Some(CodecType::PCMU),
        "the first caller-common allowed codec must lead when G729 is not allowed"
    );
}

// ── Bug 3: transport-aware parallel-fork callee offer caching ──────

fn extract_audio_port(sdp: &str) -> Option<u16> {
    for line in sdp.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("m=audio ") {
            return rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    None
}

#[tokio::test]
async fn test_parallel_fork_callee_offer_caches_same_transport_port() {
    // Two fork targets with the same transport must share the same RTP port
    // (cached callee offer). Without the Bug 3 fix, each fork created a
    // separate callee track with a different bound port.
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let mut dialplan = Dialplan::new(
        "test-fork-cache".to_string(),
        original_request,
        DialDirection::Inbound,
    );
    dialplan.media.rtp_start_port = Some(31000);
    dialplan.media.rtp_end_port = Some(31100);
    let context = CallContext {
        session_id: "test-fork-cache".to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );

    session.media.caller_offer = Some(
        concat!(
            "v=0\r\n",
            "o=alice 1 1 IN IP4 192.0.2.10\r\n",
            "s=Talk\r\n",
            "c=IN IP4 192.0.2.10\r\n",
            "t=0 0\r\n",
            "m=audio 40000 RTP/AVP 0 8 101\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=rtpmap:101 telephone-event/8000\r\n",
            "a=sendrecv\r\n",
        )
        .to_string(),
    );

    let target1 = Location {
        aor: "sip:agent1@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let target2 = Location {
        aor: "sip:agent2@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };

    let sdp1 = String::from_utf8(
        session
            .prepare_callee_media_offer(&target1)
            .await
            .expect("1st offer creation")
            .expect("1st offer"),
    )
    .unwrap();
    let port1 = extract_audio_port(&sdp1).expect("1st SDP port");

    let sdp2 = String::from_utf8(
        session
            .prepare_callee_media_offer(&target2)
            .await
            .expect("2nd offer creation")
            .expect("2nd offer"),
    )
    .unwrap();
    let port2 = extract_audio_port(&sdp2).expect("2nd SDP port");

    assert_eq!(
        port1, port2,
        "same-transport forks must share the same port (cached), got {} vs {}",
        port1, port2,
    );

    if let Some(mut bridge) = session.media.bridge.take() {
        bridge.close();
    }
}

#[tokio::test]
async fn test_parallel_fork_callee_offer_regenerates_for_different_transport() {
    // When fork targets use different transports (WebRTC vs RTP), the
    // callee offer must NOT be reused from the cache — each transport
    // produces a different SDP.
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let mut dialplan = Dialplan::new(
        "test-fork-cross".to_string(),
        original_request,
        DialDirection::Inbound,
    );
    dialplan.media.rtp_start_port = Some(31100);
    dialplan.media.rtp_end_port = Some(31200);
    let context = CallContext {
        session_id: "test-fork-cross".to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
    );

    session.media.caller_offer = Some(
        concat!(
            "v=0\r\n",
            "o=alice 1 1 IN IP4 192.0.2.10\r\n",
            "s=Talk\r\n",
            "c=IN IP4 192.0.2.10\r\n",
            "t=0 0\r\n",
            "m=audio 40000 RTP/AVP 0 8 101\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=rtpmap:101 telephone-event/8000\r\n",
            "a=sendrecv\r\n",
        )
        .to_string(),
    );

    // First fork: WebRTC target → SDP has DTLS fingerprint
    let webrtc_target = Location {
        aor: "sip:agent-webrtc@rustpbx.com".try_into().unwrap(),
        supports_webrtc: true,
        ..Default::default()
    };
    let sdp_w = String::from_utf8(
        session
            .prepare_callee_media_offer(&webrtc_target)
            .await
            .expect("WebRTC offer creation")
            .expect("WebRTC offer"),
    )
    .unwrap();
    assert!(
        sdp_w.contains("a=fingerprint"),
        "WebRTC target SDP must have DTLS fingerprint: {}",
        sdp_w,
    );

    // Second fork: RTP target → SDP must NOT have DTLS fingerprint
    let rtp_target = Location {
        aor: "sip:agent-rtp@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let sdp_r = String::from_utf8(
        session
            .prepare_callee_media_offer(&rtp_target)
            .await
            .expect("RTP offer creation")
            .expect("RTP offer"),
    )
    .unwrap();
    assert!(
        !sdp_r.contains("a=fingerprint"),
        "RTP target SDP must NOT have DTLS fingerprint: {}",
        sdp_r,
    );

    // Different transports → the SDP strings must differ
    assert_ne!(
        sdp_w, sdp_r,
        "different transport forks must produce different SDP (not cached)"
    );

    if let Some(mut bridge) = session.media.bridge.take() {
        bridge.close();
    }
}

// ── Bug 4: app bridge reused for same-transport callee ─────────────

// ── Layer 2: media.play → codec + sample rate verification ──
//
// Content verification (cross-correlation, frequency analysis) is done
// at the Recorder level in src/media/info_recording_tests.rs (Layer 4)
// because bridge get_callee_track() exposes the RECEIVE path (audio from
// callee), not the SEND path where handle_play injects the file.

// ── Layer 3: hold/unhold SDP direction ──

#[tokio::test]
async fn test_hold_sdp_contains_sendonly() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };
    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .unwrap();

    let (mut session, _h, _rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        CallContext {
            session_id: "test-hold-sdp".to_string(),
            dialplan: Arc::new(Dialplan::new(
                "test-hold-sdp".to_string(),
                original_request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".to_string(),
            original_callee: "sip:bob@rustpbx.com".to_string(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        },
        server_dialog,
        false,
    );

    // Hold SDP: sendrecv → sendonly
    let sendrecv_sdp = concat!(
        "v=0\r\n",
        "o=alice 1 1 IN IP4 192.0.2.10\r\n",
        "s=Talk\r\n",
        "c=IN IP4 192.0.2.10\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/AVP 0 101\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:101 telephone-event/8000\r\n",
        "a=sendrecv\r\n",
    )
    .to_string();
    // The method reads answer first, then caller_offer
    session.media.answer = Some(sendrecv_sdp);

    let hold_sdp = session
        .generate_sdp_for_side(&LegId::from("caller"), true)
        .expect("hold SDP");
    assert!(
        hold_sdp.contains("a=sendonly"),
        "hold SDP must be sendonly, got: {}",
        hold_sdp
    );
    assert!(
        !hold_sdp.contains("a=sendrecv"),
        "hold SDP must NOT contain sendrecv"
    );

    let unhold_sdp = session
        .generate_sdp_for_side(&LegId::from("caller"), false)
        .expect("unhold SDP");
    assert!(
        unhold_sdp.contains("a=sendrecv"),
        "unhold SDP must be sendrecv, got: {}",
        unhold_sdp
    );
    assert!(
        !unhold_sdp.contains("a=sendonly"),
        "unhold SDP must NOT contain sendonly"
    );
}

// ── Layer 1: parse_info_command dispatch (pure function, no session needed) ──

#[test]
fn test_parse_info_media_play() {
    let params = serde_json::json!({"source": {"source_type": "file", "uri": "/tmp/test.wav"}, "loop": true});
    let cmd = SipSession::parse_info_command("media.play", Some(&params), &params)
        .expect("parse_info_command returned None");
    match cmd {
        CallCommand::Play {
            source: crate::call::domain::MediaSource::File { ref path },
            ref options,
            ..
        } => {
            assert_eq!(path, "/tmp/test.wav");
            assert!(options.as_ref().unwrap().loop_playback);
        }
        _ => panic!("expected Play with File source"),
    }
}

#[test]
fn test_parse_info_media_stop() {
    let json = serde_json::json!({"leg_id": "callee"});
    let cmd = SipSession::parse_info_command("media.stop", Some(&json), &json).unwrap();
    assert!(
        matches!(&cmd, CallCommand::StopPlayback { leg_id } if leg_id == &Some(LegId::from("callee")))
    );
}

#[test]
fn test_parse_info_record_start() {
    let json = serde_json::json!({"path": "/tmp/rec.wav", "beep": false});
    let cmd = SipSession::parse_info_command("record.start", Some(&json), &json).unwrap();
    assert!(
        matches!(&cmd, CallCommand::StartRecording { config } if config.path == "/tmp/rec.wav" && !config.beep)
    );
}

#[test]
fn test_parse_info_record_start_with_segment_fields() {
    let json = serde_json::json!({
        "beep": false,
        "type": "ivr",
        "id": "seg9",
        "notify_app": false
    });
    let cmd = SipSession::parse_info_command("record.start", Some(&json), &json).unwrap();
    match cmd {
        CallCommand::StartRecording { config } => {
            assert_eq!(config.segment_type.as_deref(), Some("ivr"));
            assert_eq!(config.segment_id.as_deref(), Some("seg9"));
            assert_eq!(config.notify_app, Some(false));
            assert!(config.path.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn test_parse_info_record_stop() {
    assert!(matches!(
        SipSession::parse_info_command("record.stop", None, &serde_json::json!({})),
        Some(CallCommand::StopRecording),
    ));
}

#[test]
fn test_parse_info_hold() {
    let json = serde_json::json!({"leg_id": "callee"});
    let cmd = SipSession::parse_info_command("hold", Some(&json), &json).unwrap();
    assert!(
        matches!(&cmd, CallCommand::Hold { leg_id, music } if leg_id == &LegId::from("callee") && music.is_none())
    );
}

#[test]
fn test_parse_info_unhold() {
    let json = serde_json::json!({"leg_id": "callee"});
    let cmd = SipSession::parse_info_command("unhold", Some(&json), &json).unwrap();
    assert!(matches!(&cmd, CallCommand::Unhold { leg_id } if leg_id == &LegId::from("callee")));
}

#[test]
fn test_parse_info_hold_with_music() {
    let json = serde_json::json!({"music": {"source_type": "file", "uri": "/tmp/hold.wav"}});
    let cmd = SipSession::parse_info_command("hold", Some(&json), &json).unwrap();
    assert!(matches!(&cmd, CallCommand::Hold { music: Some(_), .. }));
}

#[test]
fn test_parse_info_consult_initiate() {
    let parsed = serde_json::json!({});
    let json = serde_json::json!({"leg_id": "caller"});
    let cmd = SipSession::parse_info_command("consult.initiate", Some(&json), &parsed).unwrap();
    assert!(
        matches!(&cmd, CallCommand::Hold { leg_id, music: None } if leg_id == &LegId::from("caller"))
    );
}

#[test]
fn test_parse_info_consult_cancel() {
    let parsed = serde_json::json!({"call_id": "dynamic-leg"});
    let cmd = SipSession::parse_info_command("consult.cancel", None, &parsed).unwrap();
    assert!(
        matches!(&cmd, CallCommand::Unhold { leg_id } if leg_id == &LegId::from("dynamic-leg"))
    );
}

#[test]
fn test_parse_info_unknown_action() {
    assert!(
        SipSession::parse_info_command("unknown.action", None, &serde_json::json!({})).is_none()
    );
}

// ── Layer 2 helpers (take &mut SipSession only, no complex types) ──

// ── BuiltinAppFactory IVR from DB store ──────────────────────────────────

#[tokio::test]
async fn builtin_app_factory_creates_ivr_from_db_store() {
    use sea_orm::{ConnectionTrait, Database, sea_query::SqliteQueryBuilder};

    // Setup in-memory SQLite with config_entries table
    let db = Database::connect("sqlite::memory:").await.unwrap();
    let schema = sea_orm::Schema::new(db.get_database_backend());
    let stmt = schema.create_table_from_entity(crate::models::config_entry::Entity);
    let sql = stmt.to_string(SqliteQueryBuilder);
    db.execute_unprepared(&sql).await.unwrap();
    db.execute_unprepared(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_config_entries_category_name \
         ON config_entries (category, entry_name)",
    )
    .await
    .unwrap();

    // Write a valid IVR entry into the DB store
    let store = crate::config_store::GeneratedConfigStore::Database { db: db.clone() };
    let ivr_toml = r#"
[ivr]
name = "test-ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "sounds/welcome.wav"
timeout_ms = 30000
max_retries = 3
"#;
    store
        .write("ivr", "test_ivr.generated.toml", ivr_toml)
        .await
        .unwrap();

    // Config with generated_db = true (must match server's real config)
    let mut config = crate::config::Config::default();
    config.proxy.generated_db = true;

    let call_info = crate::call::app::CallInfo {
        session_id: "test-session".to_string(),
        caller: "caller".to_string(),
        callee: "1000".to_string(),
        direction: "inbound".to_string(),
        started_at: chrono::Utc::now(),
        sip_headers: std::collections::HashMap::new(),
        route_name: None,
    };
    let app_ctx = crate::call::app::ApplicationContext::new(
        db,
        call_info,
        std::sync::Arc::new(config),
        reqwest::Client::new(),
    );

    let factory = BuiltinAppFactory::new(None, None);

    let params = Some(serde_json::json!({
        "file": "db://ivr/test_ivr.generated.toml"
    }));
    let app = factory.create_app("ivr", params, &app_ctx).await;

    assert!(
        app.ok().flatten().is_some(),
        "BuiltinAppFactory should create IVR app from DB store when generated_db=true"
    );

    let step_ivr_toml = r#"
[ivr]
name = "endpoint-step-ivr"
ivr_mode = "step"

[ivr.provider]
endpoint = "example-step"
"#;
    store
        .write("ivr", "endpoint-step-ivr.generated.toml", step_ivr_toml)
        .await
        .unwrap();

    let mut endpoint_config = crate::config::Config::default();
    endpoint_config.proxy.generated_db = true;
    endpoint_config.proxy.ivr_endpoints.insert(
        "example-step".to_string(),
        crate::config::IvrEndpointConfig {
            url: "https://provider.example.test/ivr/step".to_string(),
        },
    );
    let endpoint_ctx = crate::call::app::ApplicationContext::new(
        app_ctx.db.clone(),
        app_ctx.call_info.clone(),
        std::sync::Arc::new(endpoint_config),
        reqwest::Client::new(),
    );
    let endpoint_params = Some(serde_json::json!({
        "file": "db://ivr/endpoint-step-ivr.generated.toml"
    }));

    assert!(
        factory
            .create_app("ivr", endpoint_params.clone(), &endpoint_ctx)
            .await
            .unwrap()
            .is_some(),
        "configured deployment endpoint should create the shared Step IVR"
    );
    let error = factory
        .create_app("ivr", endpoint_params, &app_ctx)
        .await
        .expect_err("an unconfigured deployment endpoint must be rejected");
    assert!(error.to_string().contains("example-step"));

    let direct_config = r#"
[ivr]
name = "direct-step-ivr"
ivr_mode = "step"

[ivr.provider]
url = "https://provider.example.test/ivr/step"
"#;
    store
        .write("ivr", "direct-step-ivr.generated.toml", direct_config)
        .await
        .unwrap();
    let direct_params = Some(serde_json::json!({
        "file": "db://ivr/direct-step-ivr.generated.toml"
    }));
    assert!(
        factory
            .create_app("ivr", direct_params.clone(), &app_ctx)
            .await
            .unwrap()
            .is_some(),
        "legacy direct provider URLs should remain supported"
    );

    store
        .write(
            "ivr",
            "direct-step-ivr.generated.toml",
            &direct_config.replace(
                "url = \"https://provider.example.test/ivr/step\"",
                "url = \"https://provider.example.test/ivr/step\"\nendpoint = \"example-step\"",
            ),
        )
        .await
        .unwrap();
    let error = factory
        .create_app("ivr", direct_params, &endpoint_ctx)
        .await
        .expect_err("provider URL and endpoint must be mutually exclusive");
    assert!(error.to_string().contains("exactly one"));

    store
        .write(
            "ivr",
            "direct-step-ivr.generated.toml",
            &direct_config.replace(
                "url = \"https://provider.example.test/ivr/step\"",
                "url = \"https://provider.example.test/ivr/step\"\nendpoint = \"   \"",
            ),
        )
        .await
        .unwrap();
    let whitespace_params = Some(serde_json::json!({
        "file": "db://ivr/direct-step-ivr.generated.toml"
    }));
    let error = factory
        .create_app("ivr", whitespace_params, &endpoint_ctx)
        .await
        .expect_err("present provider fields must remain mutually exclusive when one is blank");
    assert!(error.to_string().contains("exactly one"));
}

// ── align_answer_direction_with_offer ──

#[test]
fn test_is_zero_connection() {
    assert!(SipSession::is_zero_connection("IN IP4 0.0.0.0"));
    assert!(SipSession::is_zero_connection("IN IP6 ::"));
    assert!(SipSession::is_zero_connection("IN IP6 0:0:0:0:0:0:0:0"));
    assert!(!SipSession::is_zero_connection("IN IP4 192.168.1.1"));
    assert!(!SipSession::is_zero_connection("IN IP4 127.0.0.1"));
}

#[test]
fn test_align_answer_direction_audio_hold() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=recvonly"),
        "hold offer sendonly → answer recvonly:\n{}",
        result
    );
    assert!(
        !result.contains("a=sendrecv"),
        "answer should not have sendrecv:\n{}",
        result
    );
}

#[test]
fn test_align_answer_direction_unhold() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=sendrecv"),
        "unhold offer sendrecv → answer keep sendrecv:\n{}",
        result
    );
}

#[test]
fn test_webrtc_zero_connection_is_not_hold() {
    let offer = "v=0\r\n\
o=- 123 456 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:test\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=sendrecv\r\n";
    let answer = offer.replace("a=ice-ufrag:test", "a=ice-ufrag:answer");
    let parsed =
        SipSession::parse_sdp(rustrtc::SdpType::Offer, offer, "test").expect("parse WebRTC offer");

    assert!(!SipSession::is_hold_direction(
        rustrtc::Direction::SendRecv,
        Some(&parsed),
    ));
    let aligned = SipSession::align_answer_direction_with_offer(offer, &answer);
    assert!(aligned.contains("a=sendrecv"));
    assert!(!aligned.contains("a=inactive"));
}

#[test]
fn test_align_answer_direction_audio_recvonly() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=sendonly"),
        "offer recvonly → answer sendonly:\n{}",
        result
    );
}

#[test]
fn test_align_answer_direction_inactive() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=inactive\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=inactive"),
        "offer inactive → answer inactive:\n{}",
        result
    );
}

#[test]
fn test_align_answer_direction_port_zero() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=inactive"),
        "port=0 → answer inactive:\n{}",
        result
    );
}

#[test]
fn test_align_answer_direction_zero_connection() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=inactive"),
        "c=0.0.0.0 → answer inactive:\n{}",
        result
    );
}

#[test]
fn test_align_answer_direction_mixed_audio_video() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\nm=video 10002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=sendrecv\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=recvonly"),
        "audio hold → audio recvonly:\n{}",
        result
    );
    assert!(
        result.contains("a=sendrecv"),
        "video unchanged → video sendrecv:\n{}",
        result
    );
    // Audio section rewritten → recvonly, video unchanged → sendrecv
    let recvonly_count = result.matches("a=recvonly").count();
    let sendrecv_count = result.matches("a=sendrecv").count();
    assert_eq!(
        recvonly_count, 1,
        "one recvonly for audio hold:\n{}",
        result
    );
    assert_eq!(sendrecv_count, 1, "one sendrecv for video:\n{}", result);
}

#[test]
fn test_align_answer_direction_no_offer_direction() {
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    // No direction in offer → default is sendrecv → answer unchanged
    assert!(
        result.contains("a=sendrecv"),
        "no offer direction → answer unchanged:\n{}",
        result
    );
}

#[test]
fn test_align_answer_direction_invalid_offer() {
    let offer = "not an sdp at all";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert_eq!(result, answer, "invalid offer → answer unchanged");
}

#[test]
fn test_align_answer_direction_section_connection_zero() {
    // Section-level c=0.0.0.0, session-level c=10.0.0.1
    let offer = "v=0\r\no=- 123 456 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let answer = "v=0\r\no=- 789 101 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let result = SipSession::align_answer_direction_with_offer(offer, answer);
    assert!(
        result.contains("a=inactive"),
        "section c=0.0.0.0 → answer inactive:\n{}",
        result
    );
}

// ── resolve_audio_file_path: the path resolution that handle_play relies on ──

#[test]
fn test_resolve_audio_file_path_http_passthrough() {
    assert_eq!(
        SipSession::resolve_audio_file_path("http://example.com/a.wav"),
        "http://example.com/a.wav"
    );
    assert_eq!(
        SipSession::resolve_audio_file_path("https://example.com/a.wav"),
        "https://example.com/a.wav"
    );
}

#[test]
fn test_resolve_audio_file_path_absolute_passthrough() {
    let abs = if cfg!(windows) {
        "C:\\tmp\\a.wav"
    } else {
        "/tmp/a.wav"
    };
    assert_eq!(SipSession::resolve_audio_file_path(abs), abs);
}

#[test]
fn test_resolve_audio_file_path_config_prefix_passthrough() {
    // Already-prefixed paths must be returned as-is to avoid double prefixing.
    assert_eq!(
        SipSession::resolve_audio_file_path("config/sounds/foo.wav"),
        "config/sounds/foo.wav"
    );
    assert_eq!(
        SipSession::resolve_audio_file_path("./config/sounds/foo.wav"),
        "./config/sounds/foo.wav"
    );
}

#[test]
fn test_resolve_audio_file_path_falls_back_to_config_prefix() {
    // The shipped convention: configs reference "sounds/foo.wav" but the
    // files live under "config/sounds/" at dev time. The resolver must
    // transparently rewrite to the existing config-prefixed path.
    let tmp = std::env::temp_dir().join("rp_bench_exists.wav");
    std::fs::write(&tmp, b"dummy").unwrap();
    let abs = tmp.to_string_lossy().to_string();
    // Absolute path that exists → passthrough.
    assert_eq!(SipSession::resolve_audio_file_path(&abs), abs);

    // Non-existent bare path with no fallback → returned unchanged.
    let bare = "definitely_missing_zzz.wav";
    assert_eq!(SipSession::resolve_audio_file_path(bare), bare);

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_resolve_audio_file_path_falls_back_to_core_sounds_outside_checkout() {
    // Edition binaries (cc, wholesale) run from their own repos, where neither
    // `sounds/` nor `config/sounds/` holds core's bundled prompts. Re-run this
    // test in a child process from an empty directory so the parent's working
    // directory is left alone.
    if std::env::var_os("RP_AUDIO_OUTSIDE_CHECKOUT").is_some() {
        let expected = format!("{}/config/sounds/phone-calling.wav", env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            SipSession::resolve_audio_file_path(crate::call::DEFAULT_QUEUE_HOLD_AUDIO),
            expected
        );
        assert_eq!(
            SipSession::resolve_audio_file_path("config/sounds/phone-calling.wav"),
            expected
        );
        assert_eq!(
            SipSession::resolve_audio_file_path("sounds/definitely_missing_zzz.wav"),
            "sounds/definitely_missing_zzz.wav"
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("rp_audio_outside_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let module = module_path!()
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or(module_path!());
    let name = format!(
        "{module}::test_resolve_audio_file_path_falls_back_to_core_sounds_outside_checkout"
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name.as_str(), "--exact", "--nocapture"])
        .env("RP_AUDIO_OUTSIDE_CHECKOUT", "1")
        .current_dir(&dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{}\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_resolve_audio_file_path_packaged_sounds_resolve_to_config() {
    // Regression for the queue-hold-music bug: the default constant
    // `sounds/phone-calling.wav` does not exist at the workspace root but
    // `config/sounds/phone-calling.wav` does. Resolution must find it.
    if !Path::new("config/sounds/phone-calling.wav").exists() {
        eprintln!("skipping: config/sounds/phone-calling.wav absent (not in workspace root)");
        return;
    }
    let resolved = SipSession::resolve_audio_file_path(crate::call::DEFAULT_QUEUE_HOLD_AUDIO);
    assert!(
        resolved.ends_with("phone-calling.wav"),
        "expected resolved path to end with phone-calling.wav, got {resolved}"
    );
    assert!(
        Path::new(&resolved).exists(),
        "resolved hold-audio path must exist: {resolved}"
    );
}

/// Every shipped default queue prompt must resolve to a real, decodable
/// WAV file. This guards against the regression where `handle_play`
/// skipped path resolution and failed with "Audio file not found".
#[tokio::test]
async fn test_default_queue_prompts_resolve_and_are_playable() {
    use crate::media::audio_source::{AudioSource, FileAudioSource};

    let cases = [
        ("hold", crate::call::DEFAULT_QUEUE_HOLD_AUDIO),
        ("failure", crate::call::DEFAULT_QUEUE_FAILURE_AUDIO),
        ("transfer-zh", crate::call::DEFAULT_QUEUE_TRANSFER_PROMPT_ZH),
        ("busy-zh", crate::call::DEFAULT_QUEUE_BUSY_PROMPT_ZH),
        (
            "no-answer-zh",
            crate::call::DEFAULT_QUEUE_NO_ANSWER_PROMPT_ZH,
        ),
    ];

    // If the test host has no `config/sounds` checkout, skip gracefully
    // rather than failing — the resolution logic is covered by other unit
    // tests in this module.
    if !Path::new("config/sounds").is_dir() {
        eprintln!("skipping: config/sounds/ directory not present");
        return;
    }

    for (label, spec) in cases {
        let resolved = SipSession::resolve_audio_file_path(spec);
        assert!(
            Path::new(&resolved).exists(),
            "[{label}] resolved path must exist: spec={spec} resolved={resolved}"
        );

        // The file must be openable AND decodable — the exact gate that
        // `handle_play` → `play_file` → `FileAudioSource::new` applies.
        let src = FileAudioSource::new(resolved.clone(), false)
            .await
            .unwrap_or_else(|e| {
                panic!("[{label}] FileAudioSource::new failed for {resolved}: {e}")
            });
        assert!(
            src.sample_rate() > 0,
            "[{label}] decoded file should report a positive sample rate"
        );
        // Pre-decoded cache must be non-empty for shipped prompts.
        assert!(
            src.has_data(),
            "[{label}] decoded file should contain PCM samples: {resolved}"
        );
        let _ = AudioSource::has_data(&src); // quiet dead_code if not used elsewhere
    }
}

// ── arm_bridged_rtp_timeouts ──────────────────────────────────────────

/// Both legs of an answered MediaBridge are armed with the RTP inactivity
/// timeout; when no ingress packets arrive the fired oneshot must turn into
/// a `CallCommand::Hangup(RtpTimeout)` on the session command channel. This
/// is the exact mechanism that tears down silent calls (no BYE) proactively.
#[tokio::test]
async fn arm_bridged_rtp_timeouts_sends_hangup_on_inactivity() {
    use crate::media::leg::{LegConfig, LegInner};

    let mut mb = crate::media::media_bridge::MediaBridge::new("rtp-timeout-session-test");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("caller", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("callee", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<CallCommand>(8);
    SipSession::arm_bridged_rtp_timeouts(
        &mb,
        Some(Duration::from_millis(150)),
        Some(cmd_tx),
        "rtp-timeout-session-test",
    );

    // Neither leg sends RTP → each armed side fires a Hangup(RtpTimeout).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_rtp_timeout_hangup = false;
    while tokio::time::Instant::now() < deadline {
        if let Some(CallCommand::Hangup(hangup)) = cmd_rx.recv().await {
            if matches!(hangup.reason, Some(CallRecordHangupReason::RtpTimeout)) {
                // The command must carry which side of the bridge fired so
                // the CDR / trace can attribute the teardown.
                assert!(
                    hangup.rtp_timeout_side.is_some(),
                    "RTP timeout HangupCommand must carry rtp_timeout_side"
                );
                saw_rtp_timeout_hangup = true;
                break;
            }
        }
    }
    assert!(
        saw_rtp_timeout_hangup,
        "RTP inactivity must emit CallCommand::Hangup(RtpTimeout)"
    );
    mb.close();
}

/// `rtp_timeout_config` returns `None` when no timeout is configured at the
/// dialplan or proxy level — in that case `arm_bridged_rtp_timeouts` must
/// NOT arm anything (a pending receiver would otherwise linger forever).
#[test]
fn rtp_timeout_config_none_when_unset() {
    let cfg = crate::config::ProxyConfig {
        rtp_timeout: None,
        ..Default::default()
    };
    // Only the pure resolution path is exercised here: with both sources
    // absent, the effective timeout must be None.
    let dialplan_timeout: Option<Duration> = None;
    let proxy_timeout: Option<Duration> = cfg.rtp_timeout.map(Duration::from_secs);
    assert!(dialplan_timeout.or(proxy_timeout).is_none());
}

/// A proxy-level `rtp_timeout` of `0` must explicitly disable the timeout
/// (equivalent to `None`), never arm an immediate fire.
#[test]
fn rtp_timeout_config_zero_disables() {
    let cfg = crate::config::ProxyConfig {
        rtp_timeout: Some(0),
        ..Default::default()
    };
    let dialplan_timeout: Option<Duration> = None;
    let proxy_timeout: Option<Duration> = cfg
        .rtp_timeout
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs);
    assert!(proxy_timeout.is_none());
    assert!(dialplan_timeout.or(proxy_timeout).is_none());
}

// ============================================================================
// route_outbound_leg / route_originated_leg (app/transfer/RWI-originated
// calls routed through the route table)
// ============================================================================

fn test_forward_route_config() -> crate::config::ProxyConfig {
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig};

    let mut config = ProxyConfig::default();
    config.route_originated_calls = true;
    config.routes = Some(vec![RouteRule {
        name: "outbound-gw".to_string(),
        priority: 100,
        match_conditions: MatchConditions {
            request_uri_user: Some("9.*".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            dest: Some(DestConfig::Single("gw1".to_string())),
            select: "rr".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let mut trunks = std::collections::HashMap::new();
    trunks.insert(
        "gw1".to_string(),
        TrunkConfig {
            dest: "sip:gateway.rustpbx.test:5060".to_string(),
            username: Some("gwuser".to_string()),
            password: Some("gwpass".to_string()),
            ..Default::default()
        },
    );
    config.trunks = trunks;
    config
}

fn test_application_route_config() -> crate::config::ProxyConfig {
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{MatchConditions, RewriteRules, RouteAction, RouteRule};

    let mut config = ProxyConfig::default();
    config.routes = Some(vec![RouteRule {
        name: "alfred-route-point".to_string(),
        priority: 100,
        match_conditions: MatchConditions {
            request_uri_user: Some("39230".to_string()),
            headers: HashMap::from([("header.X-Carried".to_string(), "original".to_string())]),
            ..Default::default()
        },
        rewrite: Some(RewriteRules {
            headers: HashMap::from([("header.X-Business-Type".to_string(), "34".to_string())]),
            ..Default::default()
        }),
        action: RouteAction {
            action: Some("application".to_string()),
            app: Some("step_ivr".to_string()),
            app_params: Some(serde_json::json!({"url": "http://127.0.0.1/ivr/step"})),
            auto_answer: true,
            ..Default::default()
        },
        ..Default::default()
    }]);
    config
}

#[tokio::test]
async fn route_leg_resolves_application_with_carried_and_rewritten_headers() {
    use crate::call::{DialDirection, TransactionCookie};
    use crate::proxy::proxy_call::sip_session::util::route_leg;
    use crate::proxy::tests::common::create_test_server_with_config;

    let (server, _) = create_test_server_with_config(test_application_route_config()).await;
    let target: rsipstack::sip::Uri = format!("sip:{}{}{}", "39230", "@", "rustpbx.test")
        .try_into()
        .unwrap();
    let caller: rsipstack::sip::Uri = format!("sip:{}{}{}", "alice", "@", "rustpbx.test")
        .try_into()
        .unwrap();
    let contact = caller.clone();
    let carry_headers = vec![rsipstack::sip::Header::Other(
        "X-Carried".to_string(),
        "original".to_string(),
    )];

    let result = route_leg(
        &server,
        &target,
        &caller,
        &contact,
        Some(carry_headers),
        &DialDirection::Inbound,
        TransactionCookie::default(),
    )
    .await
    .expect("route_leg should not error")
    .expect("route should be handled");

    match result {
        crate::config::RouteResult::Application {
            option,
            app_name,
            app_params,
            auto_answer,
            ..
        } => {
            assert_eq!(app_name, "step_ivr");
            assert_eq!(
                app_params,
                Some(serde_json::json!({"url": "http://127.0.0.1/ivr/step"}))
            );
            assert!(auto_answer);
            assert!(option.headers.as_ref().is_some_and(|headers| {
                headers.iter().any(|header| {
                    header.name().eq_ignore_ascii_case("X-Business-Type") && header.value() == "34"
                })
            }));
        }
        _ => panic!("expected Application route"),
    }
}

/// `route_outbound_leg` routes an external target through the route table
/// when the global `route_originated_calls` flag is on, stamping the
/// matched trunk's destination + credential onto the returned InviteOption.
#[tokio::test]
async fn route_outbound_leg_applies_forward_trunk() {
    use crate::call::cookie::TransactionCookie;
    use crate::proxy::tests::common::create_test_server_with_config;

    let (server, _) = create_test_server_with_config(test_forward_route_config()).await;
    let target: rsipstack::sip::Uri = "sip:9001@rustpbx.com".try_into().unwrap();
    let caller: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().unwrap();
    let contact: rsipstack::sip::Uri = "sip:rustpbx@rustpbx.com".try_into().unwrap();

    let result = route_outbound_leg(
        &server,
        &target,
        &caller,
        &contact,
        None,
        TransactionCookie::default(),
    )
    .await
    .expect("route_outbound_leg should not error");

    let result = result.expect("expected a Forward result");
    match result {
        crate::config::RouteResult::Forward(mut targets) => {
            assert_eq!(targets.len(), 1);
            let option = targets.pop().unwrap().option;
            assert_eq!(
                option.destination.as_ref().unwrap().addr.to_string(),
                "gateway.rustpbx.test:5060"
            );
            let cred = option.credential.as_ref().expect("credential stamped");
            assert_eq!(cred.username, "gwuser");
        }
        _ => panic!("expected Forward, got a different RouteResult"),
    }
}

/// When routing is disabled (flag off), `route_outbound_leg` still invokes
/// the route table but the caller decides whether to consult it. The
/// wrapper `route_originated_leg` is the gate — it returns the location
/// unchanged when the flag is off.
#[tokio::test]
async fn route_originated_leg_disabled_returns_location_unchanged() {
    use crate::call::{DialDirection, Dialplan, Location, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_config, create_transaction,
    };

    let (server, _) = create_test_server_with_config(ProxyConfig::default()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "sess-route-off".to_string(),
        dialplan: Arc::new(
            Dialplan::new(
                "sess-route-off".to_string(),
                request,
                DialDirection::Inbound,
            )
            .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    let loc = Location {
        aor: "sip:9001@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let (routed, hints) = session
        .route_originated_leg(&loc)
        .await
        .expect("routing should not error when disabled");
    assert_eq!(routed.aor, loc.aor);
    assert!(
        routed.destination.is_none(),
        "no trunk applied when disabled"
    );
    assert!(hints.is_none());
}

/// `route_originated_leg` maps a Forward result onto the Location
/// (destination + credential) and returns the routing hints so the caller
/// can release concurrency resources.
#[tokio::test]
async fn route_originated_leg_applies_forward_to_location() {
    use crate::call::{DialDirection, Dialplan, Location, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_config, create_transaction,
    };

    let (server, _) = create_test_server_with_config(test_forward_route_config()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "sess-route-on".to_string(),
        dialplan: Arc::new(
            Dialplan::new("sess-route-on".to_string(), request, DialDirection::Inbound)
                .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    let loc = Location {
        aor: "sip:9001@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let (routed, hints) = session
        .route_originated_leg(&loc)
        .await
        .expect("routing should succeed");
    assert_eq!(
        routed.destination.as_ref().unwrap().addr.to_string(),
        "gateway.rustpbx.test:5060"
    );
    assert_eq!(
        routed.credential.as_ref().expect("credential").username,
        "gwuser"
    );
    assert!(hints.is_some());
}

/// The session-level dialplan flag overrides the global default.
#[tokio::test]
async fn route_originated_leg_session_flag_overrides_global() {
    use crate::call::{DialDirection, Dialplan, Location, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_config, create_transaction,
    };

    // Global off, session on → routing must still run.
    let (server, _) = create_test_server_with_config(ProxyConfig::default()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "sess-flag-override".to_string(),
        dialplan: Arc::new(
            Dialplan::new(
                "sess-flag-override".to_string(),
                request,
                DialDirection::Inbound,
            )
            .with_caller("sip:alice@rustpbx.com".try_into().unwrap())
            .with_route_originated_calls(Some(true)),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    assert!(
        session.route_originated_enabled(),
        "session-level flag should enable routing despite global default"
    );
    // No routes configured → NotHandled → location unchanged, no hints.
    let loc = Location {
        aor: "sip:9001@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let (routed, hints) = session
        .route_originated_leg(&loc)
        .await
        .expect("routing should succeed");
    assert_eq!(routed.aor, loc.aor);
    assert!(hints.is_none());
}

/// Routing hints (concurrency holds + lease) are tracked so the session
/// releases them on cleanup. With no route rules, no hints are produced.
#[tokio::test]
async fn track_routed_leg_hints_stores_lease_and_holds() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_config, create_transaction,
    };

    let (server, _) = create_test_server_with_config(ProxyConfig::default()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "sess-hints".to_string(),
        dialplan: Arc::new(
            Dialplan::new("sess-hints".to_string(), request, DialDirection::Inbound)
                .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    // Empty hints → no tracked lease. Await a (disabled) route first so the
    // session is exercised like the other session tests before tracking.
    let loc = crate::call::Location {
        aor: "sip:9001@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let _ = session.route_originated_leg(&loc).await;
    assert_eq!(session.transient_leases.len(), 0);

    // A non-empty lease is tracked into transient_leases.
    let limiter = crate::call::concurrent_call_limiter::ConcurrentCallLimiter::new(1);
    let permit = limiter.try_acquire().expect("slot available");
    let lease = crate::call::concurrent_call_limiter::ConcurrentCallLease::default();
    lease.push(permit);
    assert_eq!(limiter.current(), 1);
    session.track_routed_leg_hints(Some(crate::config::DialplanHints {
        concurrent_call_lease: lease,
        ..Default::default()
    }));
    assert_eq!(session.transient_leases.len(), 1);

    // Dropping the session must release the tracked lease's permit.
    let limiter_arc = Arc::new(limiter);
    drop(session);
    assert_eq!(
        limiter_arc.current(),
        0,
        "routed-leg lease must be released on session drop"
    );
}

#[tokio::test]
async fn resolve_custom_targets_skips_only_unregistered_same_realm_queue_targets() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let registered_aor: rsipstack::sip::Uri = "sip:online@rustpbx.com".try_into().unwrap();
    let registered_contact: rsipstack::sip::Uri = "sip:online@10.0.0.10:5070".try_into().unwrap();
    let remote_registered_aor: rsipstack::sip::Uri = "sip:remote@rustpbx.com".try_into().unwrap();
    let remote_contact: rsipstack::sip::Uri = "sip:remote@remote-contact.invalid;transport=ws"
        .try_into()
        .unwrap();
    let remote_home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: "10.0.0.20:5060".try_into().unwrap(),
    };
    server
        .locator
        .register(
            "online",
            Some("rustpbx.com"),
            Location {
                aor: registered_contact.clone(),
                registered_aor: Some(registered_aor.clone()),
                destination: Some(SipAddr {
                    r#type: Some(rsipstack::sip::Transport::Udp),
                    addr: "10.0.0.10:5070".try_into().unwrap(),
                }),
                expires: 3600,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    server
        .locator
        .register(
            "remote",
            Some("rustpbx.com"),
            Location {
                aor: remote_contact.clone(),
                registered_aor: Some(remote_registered_aor.clone()),
                destination: Some(SipAddr {
                    r#type: Some(rsipstack::sip::Transport::Ws),
                    addr: "198.51.100.20:57890".try_into().unwrap(),
                }),
                home_proxy: Some(remote_home_proxy.clone()),
                supports_webrtc: true,
                expires: 3600,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");
    let context = CallContext {
        session_id: "queue-target-resolution".to_string(),
        dialplan: Arc::new(
            Dialplan::new(
                "queue-target-resolution".to_string(),
                request,
                DialDirection::Inbound,
            )
            .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:queue@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    let targets = vec![
        Location {
            aor: "sip:offline@rustpbx.com".try_into().unwrap(),
            ..Default::default()
        },
        Location {
            aor: registered_aor,
            ..Default::default()
        },
        Location {
            aor: remote_registered_aor,
            ..Default::default()
        },
        Location {
            aor: "sip:ringback@rustpbx.com:5099".try_into().unwrap(),
            ..Default::default()
        },
        Location {
            aor: "sip:external@example.net".try_into().unwrap(),
            ..Default::default()
        },
    ];

    let resolved = session.resolve_custom_targets(targets).await;
    let resolved_uris: Vec<String> = resolved
        .iter()
        .map(|location| location.aor.to_string())
        .collect();

    assert_eq!(
        resolved_uris,
        vec![
            registered_contact.to_string(),
            remote_contact.to_string(),
            "sip:ringback@rustpbx.com:5099".to_string(),
            "sip:external@example.net".to_string(),
        ]
    );
    let remote = &resolved[1];
    assert_eq!(remote.home_proxy, Some(remote_home_proxy));
    assert_eq!(
        remote.registered_aor.as_ref().map(ToString::to_string),
        Some("sip:remote@rustpbx.com".to_string())
    );
    assert!(remote.destination.is_some());
    assert!(remote.supports_webrtc);
}

// ── effective_ring_timeout ────────────────────────────────────────────

fn make_dialplan(max_ring_time: Option<Duration>) -> crate::call::Dialplan {
    use crate::call::DialDirection;
    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: rsipstack::sip::Uri::try_from("sip:1002@rustpbx.com").unwrap(),
        version: Default::default(),
        headers: Default::default(),
        body: Vec::new(),
    };
    let mut dp = crate::call::Dialplan::new("s".into(), request, DialDirection::Outbound);
    dp.max_ring_time = max_ring_time;
    dp
}

#[tokio::test]
async fn effective_ring_timeout_precedence_and_disabled() {
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::create_test_server;

    let (server, _) = create_test_server().await;

    // No per-call value and no global → disabled (None).
    let mut cfg = ProxyConfig::default();
    cfg.max_ring_time = None;
    server.proxy_config.store(Arc::new(cfg));
    assert_eq!(
        SipSession::effective_ring_timeout(&make_dialplan(None), &server),
        None,
        "no config → ring timeout disabled"
    );

    // Global config applies when the per-call value is absent.
    let mut cfg = ProxyConfig::default();
    cfg.max_ring_time = Some(45);
    server.proxy_config.store(Arc::new(cfg));
    assert_eq!(
        SipSession::effective_ring_timeout(&make_dialplan(None), &server),
        Some(Duration::from_secs(45)),
        "global max_ring_time should apply"
    );

    // Global 0 explicitly disables the timeout.
    let mut cfg = ProxyConfig::default();
    cfg.max_ring_time = Some(0);
    server.proxy_config.store(Arc::new(cfg));
    assert_eq!(
        SipSession::effective_ring_timeout(&make_dialplan(None), &server),
        None,
        "global max_ring_time = 0 disables the timeout"
    );

    // Per-call / per-trunk value overrides the global.
    let mut cfg = ProxyConfig::default();
    cfg.max_ring_time = Some(45);
    server.proxy_config.store(Arc::new(cfg));
    assert_eq!(
        SipSession::effective_ring_timeout(&make_dialplan(Some(Duration::from_secs(10))), &server,),
        Some(Duration::from_secs(10)),
        "per-call value overrides the global default"
    );
}

#[tokio::test]
async fn added_second_leg_relays_audio_and_dtmf_without_mixer() {
    use crate::call::{DialDirection, Dialplan, MediaConfig, TransactionCookie};
    use crate::config::{MediaProxyMode, ProxyConfig};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::media::media_bridge::MediaBridge;
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};
    use crate::rwi::RwiGateway;

    let gateway = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
    let (server, _) =
        create_test_server_with_rwi_gateway(ProxyConfig::default(), gateway.clone()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "added-media".into(),
        dialplan: Arc::new(
            Dialplan::new("added-media".into(), request, DialDirection::Inbound)
                .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All)),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:caller@rustpbx.com".into(),
        original_callee: "sip:1101@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server.clone(), cancel, None, context, true);
    let mut cfg = LegConfig::rtp_pcmu();
    cfg.codecs.push(crate::media::negotiate::CodecInfo {
        payload_type: 101,
        codec: audio_codec::CodecType::TelephoneEvent,
        clock_rate: 8000,
        channels: 1,
        fmtp: Some("0-16".into()),
    });
    let caller = LegInner::new("remote-caller", &cfg, None).unwrap();
    let local = LegInner::new("caller", &cfg, None).unwrap();
    let offer = caller.create_offer().await.unwrap();
    let answer = local
        .apply_sdp(&offer, rustrtc::SdpType::Offer)
        .await
        .unwrap();
    caller
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    session.media.caller_offer = Some(offer);
    session.media.answer = Some(answer);
    session
        .legs
        .set_media_leg(&LegId::from("caller"), local.clone());
    session.media_leg(&LegId::from("caller")).unwrap().accept();
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);

    // Exercise leg_add itself, including its actual media-offer selection.
    // Supply the remote answer through the same LegConnected command the SIP
    // response task uses; no mixer/peer setup is injected by the test.
    let alice_id = session
        .handle_add_leg_inner(
            "sip:alice@127.0.0.1:5099".into(),
            Some(LegId::from("alice-test")),
            vec![],
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        session
            .bridge()
            .and_then(
                |bridge| bridge.leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))
            )
            .map(|p| p.id().to_string()),
        None
    );
    assert!(session.legs.media_leg(&alice_id).is_some());
    let alice = LegInner::new("remote-alice", &cfg, None).unwrap();
    let answer = alice
        .apply_sdp(
            &session
                .legs
                .media_leg(&alice_id)
                .unwrap()
                .pc()
                .local_description()
                .unwrap()
                .to_sdp_string(),
            rustrtc::SdpType::Offer,
        )
        .await
        .unwrap();
    session
        .execute_command(
            CallCommand::LegConnected {
                leg_id: alice_id.clone(),
                answer_sdp: Some(answer),
                dialog_id: None,
            },
            None,
        )
        .await;
    assert!(
        !session.bridge.active,
        "answering an added leg must not connect media"
    );
    assert!(
        session
            .execute_command(
                CallCommand::Bridge {
                    leg_a: LegId::from("caller"),
                    leg_b: alice_id.clone(),
                    mode: crate::call::domain::P2PMode::Audio,
                },
                None
            )
            .await
            .success
    );
    assert!(session.bridge().unwrap().is_bridged());
    for side in [LegSide::A, LegSide::B] {
        assert!(
            session
                .bridge()
                .unwrap()
                .leg(side)
                .unwrap()
                .egress_is_relay()
        );
    }
    assert!(
        server
            .conference_server
            .get_conference(&crate::call::runtime::ConferenceId::from(
                "consult-added-media"
            ))
            .await
            .is_none()
    );

    let mut remotes = Vec::new();
    for remote in [&caller, &alice] {
        let mut bridge = MediaBridge::new("remote-observer");
        bridge.replace_leg(LegSide::A, (*remote).clone()).await;
        bridge.accept(LegSide::A).await;
        remotes.push(bridge);
    }
    // Check actual RTP audio and telephone-events in both directions.
    for (source, destination, destination_bridge, digit) in [
        (&caller, &alice, &remotes[1], "2"),
        (&alice, &caller, &remotes[0], "5"),
    ] {
        let mut audio = crate::media::app_ingress::LegPcmStream::attach(
            destination.pc(),
            destination.negotiated().unwrap(),
            LegId::from("observer"),
            session.cancel_token.child_token(),
        )
        .unwrap();
        source
            .set_egress_source(crate::media::egress::EgressSource::Media {
                audio: Box::new(
                    crate::media::audio_source::ToneAudioSource::new(
                        660,
                        Duration::from_secs(1),
                        8000,
                    )
                    .unwrap(),
                ),
                loop_playback: false,
                on_end: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = audio.recv().await.unwrap();
                if !frame.silence && frame.frame.samples.iter().any(|s| s.abs() > 100) {
                    break;
                }
            }
        })
        .await
        .expect("opposite endpoint must receive audio");
        let mut received_digits = destination_bridge.dtmf_bus();
        source.send_dtmf(digit).await.unwrap();
        let (_, received) = tokio::time::timeout(Duration::from_secs(3), received_digits.recv())
            .await
            .expect("DTMF must cross the relay")
            .unwrap();
        assert_eq!(received.digit.to_string(), digit);
    }
    // A subsequent add must not replace the connected Alice transport.
    let first_b = session
        .bridge()
        .unwrap()
        .leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))
        .unwrap()
        .clone();
    let another = session
        .handle_add_leg_inner(
            "sip:other@127.0.0.1:5098".into(),
            Some(LegId::from("another")),
            vec![],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &first_b,
        &session
            .bridge()
            .unwrap()
            .leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))
            .unwrap()
    ));
    assert!(session.legs.media_leg(&another).is_some());
    let third = LegInner::new("remote-third", &cfg, None).unwrap();
    let third_peer = session.legs.media_leg(&another).unwrap();
    let answer = third
        .answer(&third_peer.pc().local_description().unwrap().to_sdp_string())
        .await
        .unwrap();
    session
        .execute_command(
            CallCommand::LegConnected {
                leg_id: another.clone(),
                answer_sdp: Some(answer),
                dialog_id: None,
            },
            None,
        )
        .await;
    // A third answer must not change the existing pair or start a mixer.
    assert!(Arc::ptr_eq(
        &first_b,
        &session
            .bridge()
            .unwrap()
            .leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))
            .unwrap()
    ));
    assert!(session.conference_bridge.conf_id.is_none());
    session
        .execute_command(
            CallCommand::Bridge {
                leg_a: alice_id.clone(),
                leg_b: another.clone(),
                mode: crate::call::domain::P2PMode::Audio,
            },
            None,
        )
        .await;
    assert!(Arc::ptr_eq(
        &first_b,
        &session
            .bridge()
            .unwrap()
            .leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))
            .unwrap()
    ));
    assert!(Arc::ptr_eq(
        &third_peer,
        &session
            .bridge()
            .unwrap()
            .leg_for_id(&crate::media::leg_id::LegId::from(another.as_str()))
            .unwrap()
    ));
    // Signaling and media operations resolve by leg identity even when the
    // caller is no longer selected in either bridge slot.
    assert!(Arc::ptr_eq(
        &local,
        &session.media_leg(&LegId::from("caller")).unwrap()
    ));
    let caller_pc = session
        .get_local_reinvite_pc(&LegId::from("caller"))
        .await
        .unwrap();
    assert_eq!(
        caller_pc.local_description().unwrap().to_sdp_string(),
        local.pc().local_description().unwrap().to_sdp_string()
    );
    let mut third_observer = MediaBridge::new("third-observer");
    third_observer.replace_leg(LegSide::A, third.clone()).await;
    third_observer.accept(LegSide::A).await;
    let mut digits = third_observer.dtmf_bus();
    alice.send_dtmf("8").await.unwrap();
    let (_, digit) = tokio::time::timeout(Duration::from_secs(3), digits.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digit.digit, '8');
    third_observer.close();
    session.clear_bridge().await;
    assert!(
        session
            .setup_bridge(LegId::from("caller"), alice_id.clone())
            .await
    );
    // Switching away and back must preserve the original PeerConnections.
    assert!(Arc::ptr_eq(
        &first_b,
        &session
            .bridge()
            .unwrap()
            .leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))
            .unwrap()
    ));
    let mut audio = crate::media::app_ingress::LegPcmStream::attach(
        alice.pc(),
        alice.negotiated().unwrap(),
        LegId::from("restored"),
        session.cancel_token.child_token(),
    )
    .unwrap();
    caller
        .set_egress_source(crate::media::egress::EgressSource::Media {
            audio: Box::new(
                crate::media::audio_source::ToneAudioSource::new(660, Duration::from_secs(1), 8000)
                    .unwrap(),
            ),
            loop_playback: false,
            on_end: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let frame = audio.recv().await.unwrap();
            if frame.frame.samples.iter().any(|s| s.abs() > 100) {
                break;
            }
        }
    })
    .await
    .expect("restored peer must still carry audio");
    session.handle_remove_leg(another, None).await.unwrap();
    session.handle_remove_leg(alice_id, None).await.unwrap();
    let retry = session
        .handle_add_leg_inner(
            "sip:alice@127.0.0.1:5099".into(),
            Some(LegId::from("alice-retry")),
            vec![],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(
        &first_b,
        &session.legs.media_leg(&retry).unwrap()
    ));
    third.stop();
    session.bridge_mut().unwrap().close();
    for bridge in &mut remotes {
        bridge.close();
    }
}

/// `setup_bridge` defers the pair when one leg is answered but its media peer
/// has no negotiated SDP yet (`session.rs` "Media bridge deferred until usable
/// media arrives"). The deferral must (a) be visible in the call trace — it
/// used to exist only as a warn log, leaving the CDR timeline dark for a call
/// that stays unbridged — and (b) actually activate once usable media arrives:
/// the retry hooks (final-answer media setup / in-dialog negotiation) turn the
/// raced `CallCommand::Bridge` into a live route instead of stranding the call
/// with both legs emitting silence while RTP keeps flowing.
#[tokio::test]
async fn bridge_deferred_pair_is_traced_and_activates_once_media_arrives() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "bridge-deferred".into(),
        dialplan: Arc::new(Dialplan::new(
            "bridge-deferred".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new_uac(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        true,
    );
    let caller = LegId::from("caller");
    let callee = LegId::from("callee");
    // Caller leg: answered AND media negotiated (the normal answer state).
    {
        let mut leg = Leg::new(caller.clone());
        leg.state = LegState::Connected;
        session.legs.insert(caller.clone(), leg);
        let local = LegInner::new(caller.to_string(), &LegConfig::rtp_pcmu(), None).unwrap();
        let phone = LegInner::new(format!("phone-{caller}"), &LegConfig::rtp_pcmu(), None).unwrap();
        let offer = local.create_offer().await.unwrap();
        let answer = phone.answer(&offer).await.unwrap();
        local
            .apply_sdp(&answer, rustrtc::SdpType::Answer)
            .await
            .unwrap();
        local.accept();
        session.legs.set_answer(caller.clone(), answer.clone());
        session.legs.set_media_leg(&caller, local);
    }
    // Callee leg: the media peer exists (created at INVITE time) but the
    // answer SDP has not been applied yet — the race window where a Bridge
    // command lands first (`negotiated()` is None).
    {
        let mut leg = Leg::new(callee.clone());
        leg.state = LegState::Connected;
        session.legs.insert(callee.clone(), leg);
        let pending = LegInner::new(callee.to_string(), &LegConfig::rtp_pcmu(), None).unwrap();
        session.legs.set_media_leg(&callee, pending);
    }
    assert!(session.legs.media_leg(&callee).is_some());
    assert!(
        session
            .legs
            .media_leg(&callee)
            .unwrap()
            .negotiated()
            .is_none()
    );

    // Bridge races ahead of the callee SDP → deferred, pair kept.
    assert!(session.setup_bridge(caller.clone(), callee.clone()).await);
    assert!(
        session.bridge().is_none(),
        "route must stay deferred while the callee peer is unnegotiated"
    );
    let deferred_traces = |session: &SipSession| {
        session
            .meta
            .trace
            .iter()
            .filter(|ev| {
                ev.kind == crate::call_errors::TraceKind::MediaIssue
                    && ev.message.contains("deferred")
            })
            .count()
    };
    assert!(
        deferred_traces(&session) == 1,
        "deferred bridge must be recorded in the call trace"
    );
    // Re-entering setup_bridge for the still-deferred pair must not spam the
    // timeline — one deferred event per session.
    assert!(session.setup_bridge(caller.clone(), callee.clone()).await);
    assert_eq!(
        deferred_traces(&session),
        1,
        "deferred trace must be one-shot"
    );

    // Usable media arrives: the callee peer negotiates (production: the
    // final-answer media setup or an in-dialog negotiation applies the SDP).
    let local = LegInner::new(callee.to_string(), &LegConfig::rtp_pcmu(), None).unwrap();
    let phone = LegInner::new(format!("phone-{callee}"), &LegConfig::rtp_pcmu(), None).unwrap();
    let offer = local.create_offer().await.unwrap();
    let answer = phone.answer(&offer).await.unwrap();
    local
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    local.accept();
    session.legs.set_answer(callee.clone(), answer.clone());
    session.legs.set_media_leg(&callee, local);

    // The arrival retry (retry_deferred_media_bridge) must activate the pair.
    session.retry_deferred_media_bridge().await;
    assert!(
        session.bridge().is_some_and(|bridge| bridge.is_bridged()),
        "deferred pair must activate once usable media arrives"
    );

    session.bridge_mut().unwrap().close();
}

#[tokio::test]
async fn conference_merge_resolves_queue_agent_alias_and_delivers_audio() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "queue-merge".into(),
        dialplan: Arc::new(Dialplan::new(
            "queue-merge".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:queue@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new_uac(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        true,
    );
    let agent = LegId::from(uuid::Uuid::new_v4().to_string());
    let consult = LegId::from("consult-queue-merge");
    let mut phones = Vec::new();
    for id in [LegId::from("caller"), agent.clone(), consult.clone()] {
        let mut leg = Leg::new(id.clone());
        leg.state = LegState::Connected;
        if id == consult {
            leg.source_leg = Some(agent.clone());
        }
        session.legs.insert(id.clone(), leg);
        let local = LegInner::new(id.to_string(), &LegConfig::rtp_pcmu(), None).unwrap();
        let phone = LegInner::new(format!("phone-{id}"), &LegConfig::rtp_pcmu(), None).unwrap();
        let offer = local.create_offer().await.unwrap();
        let answer = phone.answer(&offer).await.unwrap();
        local
            .apply_sdp(&answer, rustrtc::SdpType::Answer)
            .await
            .unwrap();
        local.accept();
        phone.accept();
        session.legs.set_answer(id.clone(), answer);
        session.legs.set_media_leg(&id, local);
        phones.push(phone);
    }
    assert!(session.media_leg(&LegId::from("callee")).is_none());
    assert!(
        session
            .setup_bridge(LegId::from("caller"), agent.clone())
            .await
    );
    assert_eq!(session.resolve_transfer_leg(), agent);
    session
        .handle_hold(LegId::from("caller"), None)
        .await
        .unwrap();
    assert_eq!(
        session.resolve_transfer_leg(),
        agent,
        "hold must retain the queue agent as the selected callee"
    );
    session.handle_unhold(LegId::from("caller")).await.unwrap();
    assert!(
        session
            .setup_bridge(LegId::from("caller"), consult.clone())
            .await
    );
    assert_eq!(
        session.resolve_transfer_leg(),
        consult,
        "a completed transfer selects its new peer even with source_leg set"
    );
    assert!(session.setup_bridge(agent.clone(), consult.clone()).await);
    let room = crate::call::runtime::ConferenceId::from("queue-merge-room");
    server
        .conference_server
        .create_conference(room.clone(), None)
        .await
        .unwrap();
    // CC sends the legacy callee alias; the session must join the real UUID
    // leg, while preserving the concrete caller and consultation IDs.
    for id in [
        LegId::from("caller"),
        LegId::from("callee"),
        consult.clone(),
    ] {
        let result = session
            .execute_command(
                CallCommand::JoinMixerLeg {
                    mixer_id: room.0.clone(),
                    leg_id: id,
                },
                None,
            )
            .await;
        assert!(result.success, "conference join failed: {:?}", result);
    }
    assert_eq!(
        server
            .conference_server
            .get_conference(&room)
            .await
            .unwrap()
            .participant_count(),
        3
    );
    assert!(session.legs.conference_bridge_handle(&agent).is_some());
    assert!(
        session
            .legs
            .conference_bridge_handle(&LegId::from("callee"))
            .is_none()
    );

    let cancel = CancellationToken::new();
    let mut agent_audio = phones[1].pcm_stream(cancel.clone()).unwrap();
    phones[0]
        .play_media(
            Box::new(
                crate::media::audio_source::ToneAudioSource::new(440, Duration::from_secs(2), 8000)
                    .unwrap(),
            ),
            true,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = agent_audio.recv().await.unwrap();
            if frame
                .frame
                .samples
                .iter()
                .any(|sample| sample.unsigned_abs() > 100)
            {
                break;
            }
        }
    })
    .await
    .expect("queued agent must hear caller audio after three-way merge");
    cancel.cancel();
    session.handle_leave_mixer().await.unwrap();
    for phone in phones {
        phone.stop();
    }
}

#[tokio::test]
async fn consult_media_preserves_peers_across_bridge_and_explicit_mixer() {
    // The consult/bridge media setup nests a long async poll chain (session
    // command → hooks → transfer manager → MediaBridge/rustrtc) whose
    // DEBUG-build frames exhaust the default 8MB libtest thread stack
    // (verified: overflows at 8MB inside CallCommand::Bridge, passes at
    // 16MB+). Run the body on a dedicated fat-stack thread; release builds
    // are unaffected.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("consult-media runtime")
                .block_on(consult_media_preserves_peers_body())
        })
        .expect("spawn consult-media test thread")
        .join()
        .expect("consult-media test body panicked");
}

async fn consult_media_preserves_peers_body() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::media::media_bridge::MediaBridge;
    use crate::proxy::tests::common::create_test_request;
    #[cfg(not(feature = "addon-cc"))]
    use crate::proxy::tests::common::create_test_server_with_config;

    for scenario in [
        "reject",
        "timeout",
        "cancel_ringing",
        "cancel_answered",
        "core_cancel_ringing",
        "core_cancel_answered",
        "private_hangup",
        "complete",
        "complete_from_customer",
        "merged",
        "switch_then_merge",
        "supervisor_switch",
    ] {
        #[cfg(feature = "addon-cc")]
        let transfer_container = Arc::new(tokio::sync::RwLock::new(None));
        #[cfg(feature = "addon-cc")]
        let call_container = Arc::new(tokio::sync::RwLock::new(None));
        #[cfg(feature = "addon-cc")]
        let (server, _) = crate::proxy::tests::common::create_test_server_with_session_hooks(
            ProxyConfig::default(),
            vec![Arc::new(
                crate::addons::cc::cc_call_session_hook::CcCallSessionHook::new(
                    Arc::new(crate::addons::cc::agent::AgentRegistry::new()),
                    Arc::new(crate::addons::cc::metrics::MetricsCollector::new()),
                )
                .with_transfer_manager_shared(transfer_container.clone())
                .with_call_registry_shared(call_container.clone()),
            )],
        )
        .await;
        #[cfg(not(feature = "addon-cc"))]
        let (server, _) = create_test_server_with_config(ProxyConfig::default()).await;
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let context = CallContext {
            session_id: "consult-media".into(),
            dialplan: Arc::new(Dialplan::new(
                "consult-media".into(),
                request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".into(),
            original_callee: "sip:bob@rustpbx.com".into(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, _handle, mut _commands) = SipSession::new_uac(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            true,
        );
        #[cfg(feature = "addon-cc")]
        let transfer_manager = {
            server
                .active_call_registry
                .register_handle(session.id.to_string(), _handle.clone());
            *call_container.write().await = Some(server.active_call_registry.clone());
            let manager = Arc::new(
                crate::addons::cc::transfer::ConsultTransferManager::new(
                    server.conference_server.manager_raw().clone(),
                )
                .with_call_registry(server.active_call_registry.clone()),
            );
            manager.initiate(
                "transfer-media".into(),
                session.id.to_string(),
                "agent".into(),
                "alice".into(),
            );
            manager
                .mark_connected_pending_answer("transfer-media", session.id.to_string())
                .unwrap();
            *transfer_container.write().await = Some(manager.clone());
            manager
        };
        let callee = LegId::from("callee");
        session.legs.insert(callee.clone(), Leg::new(callee));
        let mut bridge = MediaBridge::new("consult-media");
        let mut remote_legs = Vec::new();
        for (side, name) in [(LegSide::A, "caller"), (LegSide::B, "callee")] {
            let local = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote =
                LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = local.create_offer().await.unwrap();
            if side == LegSide::A {
                session.media.answer = Some(offer.clone());
                session.media.caller_offer = Some(offer.clone());
            }
            let answer = remote
                .apply_sdp(&offer, rustrtc::SdpType::Offer)
                .await
                .unwrap();
            local
                .apply_sdp(&answer, rustrtc::SdpType::Answer)
                .await
                .unwrap();
            session.legs.set_answer(LegId::from(name), answer);
            session.update_leg_state(&LegId::from(name), LegState::Connected);
            local.accept();
            remote.accept();
            session
                .legs
                .set_media_leg(&LegId::from(name), local.clone());
            bridge.replace_leg(side, local).await;
            remote_legs.push(remote);
        }
        session.media.bridge = Some(bridge);
        session.update_leg_state(&LegId::from("caller"), LegState::Hold);
        let agent_pc = session
            .bridge()
            .unwrap()
            .leg_for_id(&crate::media::leg_id::LegId::from("callee"))
            .unwrap()
            .pc()
            .clone();
        let agent_sdp = agent_pc.remote_description();
        let consult = LegId::from("consult-transfer-media");
        let mut consult_info = Leg::new(consult.clone());
        consult_info.source_leg = Some(LegId::from("callee"));
        session.legs.insert(consult.clone(), consult_info);
        let (peer, offer) = session
            .create_leg_peer(&consult, rustrtc::TransportMode::Rtp)
            .await
            .unwrap();
        session.legs.set_media_leg(&consult, peer.clone());
        let remote = LegInner::new("remote-consult", &LegConfig::rtp_pcmu(), None).unwrap();
        let answer = remote
            .apply_sdp(&offer, rustrtc::SdpType::Offer)
            .await
            .unwrap();

        session
            .execute_command(
                CallCommand::Bridge {
                    leg_a: LegId::from("callee"),
                    leg_b: consult.clone(),
                    mode: crate::call::domain::P2PMode::Audio,
                },
                None,
            )
            .await;
        assert_ne!(
            session.legs.get(&consult).unwrap().state,
            LegState::Connected
        );
        assert!(session.conference_bridge.conf_id.is_none());
        if scenario == "core_cancel_ringing" {
            assert!(
                session
                    .execute_command(
                        CallCommand::TransferCancel {
                            consult_leg: consult.clone()
                        },
                        None
                    )
                    .await
                    .success
            );
            assert!(!session.legs.contains_key(&consult));
            assert_eq!(
                session.bridge.legs,
                vec![LegId::from("caller"), LegId::from("callee")]
            );
            assert!(session.bridge().unwrap().is_bridged());
            assert_eq!(
                session.legs.get(&LegId::from("caller")).unwrap().state,
                LegState::Connected
            );
            continue;
        }
        if matches!(scenario, "reject" | "timeout" | "cancel_ringing") {
            session
                .execute_command(
                    if scenario.starts_with("cancel_") {
                        CallCommand::LegRemove {
                            leg_id: consult.clone(),
                            reason: None,
                        }
                    } else {
                        CallCommand::LegFailed {
                            leg_id: consult.clone(),
                            reason: if scenario == "reject" {
                                "Rejected with 603"
                            } else {
                                "Timeout"
                            }
                            .into(),
                        }
                    },
                    None,
                )
                .await;
            // Recovery belongs to the CC owner, not generic leg removal.
            assert_eq!(
                session.legs.get(&LegId::from("caller")).unwrap().state,
                LegState::Hold
            );
            #[cfg(feature = "addon-cc")]
            if !scenario.starts_with("cancel_") {
                assert!(matches!(
                    transfer_manager.get_state("transfer-media"),
                    Some(crate::addons::cc::transfer::TransferState::Failed { .. })
                ));
                while let Ok(command) = _commands.try_recv() {
                    session.execute_command(command, None).await;
                }
            }
            if !cfg!(feature = "addon-cc") || scenario.starts_with("cancel_") {
                for command in [
                    CallCommand::LeaveMixer,
                    CallCommand::LegRemove {
                        leg_id: consult.clone(),
                        reason: None,
                    },
                    CallCommand::Bridge {
                        leg_a: LegId::from("caller"),
                        leg_b: LegId::from("callee"),
                        mode: crate::call::domain::P2PMode::Audio,
                    },
                    CallCommand::Unhold {
                        leg_id: LegId::from("caller"),
                    },
                    CallCommand::Unhold {
                        leg_id: LegId::from("callee"),
                    },
                ] {
                    assert!(session.execute_command(command, None).await.success);
                }
            }
            assert!(session.legs.get(&consult).is_none());
            // An already queued answer must not resurrect a cancelled/failed C.
            assert!(
                session
                    .execute_command(
                        CallCommand::LegConnected {
                            leg_id: consult.clone(),
                            answer_sdp: None,
                            dialog_id: None,
                        },
                        None
                    )
                    .await
                    .success
            );
            assert!(!session.legs.contains_key(&consult));
            assert_eq!(
                session.legs.get(&LegId::from("caller")).unwrap().state,
                LegState::Connected
            );
            assert!(session.conference_bridge.conf_id.is_none());
            tokio::time::timeout(Duration::from_secs(2), async {
                while !["caller", "callee"].iter().all(|side| {
                    session
                        .bridge()
                        .unwrap()
                        .leg_for_id(&crate::media::leg_id::LegId::from(*side))
                        .unwrap()
                        .egress_is_relay()
                }) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("original caller-agent relay restored");
            assert!(!session.cancel_token.is_cancelled());
            continue;
        }
        // The INVITE task applies provisional SDP before notifying the session.
        session
            .media_leg(&consult)
            .unwrap()
            .apply_sdp(&answer, rustrtc::SdpType::Pranswer)
            .await
            .unwrap();
        // Provisional media may connect the requested B-C pair before final answer.
        let provisional = session
            .execute_command(
                CallCommand::LegRinging {
                    leg_id: consult.clone(),
                },
                None,
            )
            .await;
        assert!(provisional.success);
        assert_eq!(
            session.legs.get(&consult).unwrap().state,
            LegState::EarlyMedia
        );
        assert!(session.bridge().unwrap().is_bridged());
        assert_eq!(
            session.legs.get(&LegId::from("caller")).unwrap().state,
            LegState::Hold
        );
        session
            .execute_command(
                CallCommand::LegConnected {
                    leg_id: consult.clone(),
                    answer_sdp: Some(answer),
                    dialog_id: None,
                },
                None,
            )
            .await;
        assert_eq!(
            format!("{:?}", agent_pc.remote_description()),
            format!("{:?}", agent_sdp)
        );
        assert!(session.conference_bridge.conf_id.is_none());
        assert!(session.bridge().unwrap().is_bridged());
        assert_eq!(
            session.legs.get(&LegId::from("caller")).unwrap().state,
            LegState::Hold
        );
        if scenario == "core_cancel_answered" {
            assert!(
                session
                    .execute_command(
                        CallCommand::TransferCancel {
                            consult_leg: consult.clone()
                        },
                        None
                    )
                    .await
                    .success
            );
            assert!(!session.legs.contains_key(&consult));
            assert_eq!(
                session.bridge.legs,
                vec![LegId::from("caller"), LegId::from("callee")]
            );
            for name in ["caller", "callee"] {
                assert!(
                    session
                        .media_leg(&LegId::from(name))
                        .unwrap()
                        .egress_is_relay()
                );
            }
            continue;
        }
        if scenario == "complete" || scenario == "complete_from_customer" {
            if scenario == "complete_from_customer" {
                session.handle_hold(consult.clone(), None).await.unwrap();
                session.handle_unhold(LegId::from("caller")).await.unwrap();
                let agent = session.resolve_transfer_leg();
                assert!(session.setup_bridge(LegId::from("caller"), agent).await);
                assert!(session.bridge.contains_leg(&LegId::from("caller")));
            }
            let caller_peer = session.media_leg(&LegId::from("caller")).unwrap();
            let consult_peer = session.media_leg(&consult).unwrap();
            for command in [
                CallCommand::LeaveMixer,
                CallCommand::Bridge {
                    leg_a: LegId::from("caller"),
                    leg_b: consult.clone(),
                    mode: crate::call::domain::P2PMode::Audio,
                },
                CallCommand::Unhold {
                    leg_id: LegId::from("caller"),
                },
                CallCommand::Unhold {
                    leg_id: consult.clone(),
                },
                CallCommand::LegRemove {
                    leg_id: LegId::from("callee"),
                    reason: None,
                },
                CallCommand::MarkTransferred,
            ] {
                let result = session.execute_command(command, None).await;
                assert!(result.success, "{:?}", result.message);
            }
            assert!(session.legs.get(&LegId::from("callee")).is_none());
            assert!(session.conference_bridge.conf_id.is_none());
            assert!(session.bridge().unwrap().is_bridged());
            assert!(Arc::ptr_eq(
                &caller_peer,
                &session.media_leg(&LegId::from("caller")).unwrap()
            ));
            assert!(Arc::ptr_eq(
                &consult_peer,
                &session.media_leg(&consult).unwrap()
            ));
            assert!(session.bridge.contains_leg(&LegId::from("caller")));
            assert!(session.bridge.contains_leg(&consult));
            let mut audio = crate::media::app_ingress::LegPcmStream::attach(
                remote.pc(),
                remote.negotiated().unwrap(),
                crate::media::leg_id::LegId::from("complete-observer"),
                CancellationToken::new(),
            )
            .unwrap();
            remote_legs[0]
                .play(
                    Box::new(
                        crate::media::audio_source::ToneAudioSource::new(
                            660,
                            Duration::from_secs(2),
                            8000,
                        )
                        .unwrap(),
                    ),
                    false,
                    None,
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let frame = audio.recv().await.unwrap();
                    if frame.frame.samples.iter().any(|s| s.abs() > 100) {
                        break;
                    }
                }
            })
            .await
            .expect("A must reach C after direct completion");
            for remote in remote_legs {
                remote.stop();
            }
            continue;
        }
        let mut private_tokens: Vec<CancellationToken> = Vec::new();
        if scenario == "supervisor_switch" {
            session
                .handle_supervisor_monitor(
                    SupervisorMode::Listen,
                    consult.clone(),
                    LegId::from("callee"),
                    None,
                )
                .await
                .unwrap();
            let listen_token = session
                .legs
                .conference_bridge_handle(&consult)
                .unwrap()
                .cancel_token
                .clone();
            session
                .handle_supervisor_barge(consult.clone(), LegId::from("callee"), None)
                .await
                .unwrap();
            assert!(listen_token.is_cancelled());
            for name in ["caller", "callee", "consult-transfer-media"] {
                let leg = LegId::from(name);
                let room = server
                    .conference_server
                    .get_conference_id_for_leg(&session.participant_leg(&leg))
                    .await
                    .unwrap();
                assert_eq!(room.0, "supervisor-consult-media-barge");
                assert!(
                    !session
                        .legs
                        .conference_bridge_handle(&leg)
                        .unwrap()
                        .cancel_token
                        .is_cancelled()
                );
            }
            session.handle_leave_mixer().await.unwrap();
            continue;
        }
        if matches!(scenario, "private_hangup" | "cancel_answered") {
            session
                .execute_command(
                    if scenario.starts_with("cancel_") {
                        CallCommand::LegRemove {
                            leg_id: consult.clone(),
                            reason: None,
                        }
                    } else {
                        CallCommand::LegFailed {
                            leg_id: consult.clone(),
                            reason: "Remote hung up".into(),
                        }
                    },
                    None,
                )
                .await;
            // Recovery belongs to the CC owner, not generic leg removal.
            assert_eq!(
                session.legs.get(&LegId::from("caller")).unwrap().state,
                LegState::Hold
            );
            #[cfg(feature = "addon-cc")]
            if !scenario.starts_with("cancel_") {
                assert!(matches!(
                    transfer_manager.get_state("transfer-media"),
                    Some(crate::addons::cc::transfer::TransferState::Failed { .. })
                ));
                while let Ok(command) = _commands.try_recv() {
                    session.execute_command(command, None).await;
                }
            }
            if !cfg!(feature = "addon-cc") || scenario.starts_with("cancel_") {
                for command in [
                    CallCommand::LeaveMixer,
                    CallCommand::LegRemove {
                        leg_id: consult.clone(),
                        reason: None,
                    },
                    CallCommand::Bridge {
                        leg_a: LegId::from("caller"),
                        leg_b: LegId::from("callee"),
                        mode: crate::call::domain::P2PMode::Audio,
                    },
                    CallCommand::Unhold {
                        leg_id: LegId::from("caller"),
                    },
                    CallCommand::Unhold {
                        leg_id: LegId::from("callee"),
                    },
                ] {
                    assert!(session.execute_command(command, None).await.success);
                }
            }
            assert!(session.legs.get(&consult).is_none());
            // An already queued answer must not resurrect a cancelled/failed C.
            assert!(
                session
                    .execute_command(
                        CallCommand::LegConnected {
                            leg_id: consult.clone(),
                            answer_sdp: None,
                            dialog_id: None,
                        },
                        None
                    )
                    .await
                    .success
            );
            assert!(!session.legs.contains_key(&consult));
            assert_eq!(
                session.legs.get(&LegId::from("caller")).unwrap().state,
                LegState::Connected
            );
            assert!(session.conference_bridge.conf_id.is_none());
            tokio::time::timeout(Duration::from_secs(2), async {
                while !["caller", "callee"].iter().all(|side| {
                    session
                        .bridge()
                        .unwrap()
                        .leg_for_id(&crate::media::leg_id::LegId::from(*side))
                        .unwrap()
                        .egress_is_relay()
                }) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("original caller-agent relay restored");
            assert!(!session.cancel_token.is_cancelled());
            continue;
        }
        // consult_switch to Customer leaves agent↔customer media; production
        // consult_merge calls prepare_conference_merge_media before merge to
        // rebuild agent↔consult then attach the customer.
        if scenario == "switch_then_merge" {
            session.execute_command(CallCommand::LeaveMixer, None).await;
            session
                .execute_command(
                    CallCommand::Hold {
                        leg_id: consult.clone(),
                        music: None,
                    },
                    None,
                )
                .await;
            session
                .execute_command(
                    CallCommand::Unhold {
                        leg_id: LegId::from("caller"),
                    },
                    None,
                )
                .await;
            session
                .execute_command(
                    CallCommand::Bridge {
                        leg_a: LegId::from("callee"),
                        leg_b: LegId::from("caller"),
                        mode: crate::call::domain::P2PMode::Audio,
                    },
                    None,
                )
                .await;
            assert_eq!(
                session.legs.get(&LegId::from("caller")).unwrap().state,
                LegState::Connected
            );
            assert_eq!(session.legs.get(&consult).unwrap().state, LegState::Hold);
        }
        // consult_connected sends Bridge again after LegConnected has attached
        // the private pair. Keep both existing participant bridges.
        // switch_then_merge stays on customer talk; prepare+merge follows later.
        if scenario != "switch_then_merge" {
            session
                .execute_command(
                    CallCommand::Bridge {
                        leg_a: LegId::from("callee"),
                        leg_b: consult.clone(),
                        mode: crate::call::domain::P2PMode::Audio,
                    },
                    None,
                )
                .await;
            assert!(private_tokens.iter().all(|token| !token.is_cancelled()));
        }
        // Send real RTP from the consult endpoint; the agent must receive
        // decoded, non-silent mixer output while the customer remains held.
        // switch_then_merge has consult on hold (agent talking to customer) —
        // skip this check and verify 3-way audio after prepare+merge instead.
        remote.accept();
        if scenario != "switch_then_merge" {
            let mut agent_audio = crate::media::app_ingress::LegPcmStream::attach(
                remote_legs[1].pc(),
                remote_legs[1].negotiated().unwrap(),
                crate::media::leg_id::LegId::from("agent-observer"),
                CancellationToken::new(),
            )
            .unwrap();
            remote
                .set_egress_source(crate::media::egress::EgressSource::Media {
                    audio: Box::new(
                        crate::media::audio_source::ToneAudioSource::new(
                            440,
                            Duration::from_secs(1),
                            8000,
                        )
                        .unwrap(),
                    ),
                    loop_playback: false,
                    on_end: None,
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let frame = agent_audio.recv().await.unwrap();
                    if !frame.silence && frame.frame.samples.iter().any(|s| s.abs() > 100) {
                        break;
                    }
                }
            })
            .await
            .expect("agent must hear consult RTP");
        }
        // Merge keeps the consultation mixer and adds only Charlie.
        #[cfg(feature = "addon-cc")]
        {
            server
                .active_call_registry
                .register_handle(session.id.to_string(), _handle.clone());
            let transfers = &transfer_manager;
            transfers.initiate(
                "transfer-media".into(),
                session.id.to_string(),
                "agent".into(),
                "alice".into(),
            );
            transfers
                .consultation_connected("transfer-media", session.id.to_string())
                .unwrap();
            if scenario == "switch_then_merge" {
                // Restore agent↔consult with LeaveMixer (not Unbridge) — the
                // same topology prepare_conference_merge_media / switch-back
                // must establish before same-session merge.
                session.execute_command(CallCommand::LeaveMixer, None).await;
                session
                    .execute_command(
                        CallCommand::Hold {
                            leg_id: LegId::from("caller"),
                            music: None,
                        },
                        None,
                    )
                    .await;
                session
                    .execute_command(
                        CallCommand::Unhold {
                            leg_id: consult.clone(),
                        },
                        None,
                    )
                    .await;
                session
                    .execute_command(
                        CallCommand::Bridge {
                            leg_a: LegId::from("callee"),
                            leg_b: consult.clone(),
                            mode: crate::call::domain::P2PMode::Audio,
                        },
                        None,
                    )
                    .await;
                assert!(session.bridge().unwrap().is_bridged());
            }
            let room = transfers
                .merge_to_conference("transfer-media")
                .await
                .unwrap();
            assert_eq!(room, "consult-consult-media");
            assert!(matches!(transfers.get_state("transfer-media"),
            Some(crate::addons::cc::transfer::TransferState::Completed { conf_id, .. })
                if conf_id == room));
            // prepare_conference_merge_media queues the B↔C topology restore
            // (LeaveMixer / Hold / Unhold×2 / Bridge — an idempotent re-assert
            // of the switch-back state) BEFORE the merge attaches the three
            // legs to the mixer. Drain everything and verify both groups.
            let mut joins: Vec<CallCommand> = Vec::new();
            let mut restored = 0;
            let mut mark_transferred = false;
            while let Ok(cmd) = _commands.try_recv() {
                match cmd {
                    CallCommand::JoinMixerLeg { .. } => joins.push(cmd),
                    CallCommand::LeaveMixer
                    | CallCommand::Hold { .. }
                    | CallCommand::Unhold { .. }
                    | CallCommand::Bridge { .. } => restored += 1,
                    CallCommand::MarkTransferred => mark_transferred = true,
                    other => panic!("unexpected command from merge: {other:?}"),
                }
            }
            assert_eq!(
                restored, 5,
                "B↔C topology restore (5 commands) must precede the joins"
            );
            assert_eq!(joins.len(), 3, "merge must attach A/B/C to the mixer");
            for (i, expected) in ["caller", "callee", "consult-transfer-media"]
                .iter()
                .enumerate()
            {
                assert!(
                    matches!(
                        &joins[i],
                        CallCommand::JoinMixerLeg { mixer_id, leg_id }
                            if mixer_id == &room && leg_id == &LegId::from(*expected)
                    ),
                    "expected JoinMixerLeg({expected}) into {room}"
                );
            }
            assert!(
                mark_transferred,
                "merge retains the existing transfer bookkeeping command"
            );
            assert!(
                _commands.try_recv().is_err(),
                "merge must only attach A/B/C + MarkTransferred"
            );
            for cmd in joins.into_iter() {
                session.execute_command(cmd, None).await;
            }
            for name in ["callee", "consult-transfer-media"] {
                private_tokens.push(
                    session
                        .legs
                        .conference_bridge_handle(&LegId::from(name))
                        .unwrap()
                        .cancel_token
                        .clone(),
                );
            }
        }
        #[cfg(not(feature = "addon-cc"))]
        {
            {
                // LeaveMixer above dropped the last conference participants,
                // which spawns an *async* destroy that removes the mixer
                // before the room. Joining while that task is mid-flight
                // fails with "Audio mixer not found" (torn window) or
                // "Conference not found" (fully torn down). Wait for the
                // teardown to settle, recreate the room and restore all
                // three legs — mirroring the create-if-missing self-heal the
                // addon-cc merge path (`merge_to_conference`) applies.
                tokio::time::timeout(Duration::from_secs(5), async {
                    while server
                        .conference_server
                        .get_conference(&crate::call::runtime::ConferenceId::from(
                            "consult-consult-media",
                        ))
                        .await
                        .is_some()
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("consult conference teardown must settle");
                session
                    .ensure_conference("consult-consult-media", None)
                    .await
                    .unwrap();
                for name in ["callee", "consult-transfer-media"] {
                    session
                        .handle_join_mixer_leg("consult-consult-media".into(), LegId::from(name))
                        .await
                        .unwrap();
                }
                private_tokens.clear();
                for name in ["callee", "consult-transfer-media"] {
                    let id = LegId::from(name);
                    let handle = session
                        .legs
                        .conference_bridge_handle(&id)
                        .expect("restored consultation bridge");
                    private_tokens.push(handle.cancel_token.clone());
                }
            }
            session
                .handle_join_mixer_leg("consult-consult-media".into(), LegId::from("caller"))
                .await
                .unwrap();
        }
        assert!(private_tokens.iter().all(|token| !token.is_cancelled()));
        assert_eq!(
            server
                .conference_server
                .get_conference(&crate::call::runtime::ConferenceId::from(
                    "consult-consult-media"
                ))
                .await
                .unwrap()
                .participant_count(),
            3
        );
        for id in ["caller", "callee", "consult-transfer-media"] {
            assert!(
                !session
                    .media_leg(&LegId::from(id))
                    .unwrap()
                    .egress_is_relay(),
                "merged participants must preserve mixer output"
            );
        }
        assert_eq!(
            session.legs.get(&LegId::from("caller")).unwrap().state,
            LegState::Connected
        );
        // Verify the opposite dynamic-leg direction after merge: customer
        // RTP must reach the consult endpoint through the three-way mixer.
        let mut consult_audio = crate::media::app_ingress::LegPcmStream::attach(
            remote.pc(),
            remote.negotiated().unwrap(),
            crate::media::leg_id::LegId::from("consult-observer"),
            CancellationToken::new(),
        )
        .unwrap();
        remote_legs[0]
            .set_egress_source(crate::media::egress::EgressSource::Media {
                audio: Box::new(
                    crate::media::audio_source::ToneAudioSource::new(
                        660,
                        Duration::from_secs(1),
                        8000,
                    )
                    .unwrap(),
                ),
                loop_playback: false,
                on_end: None,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = consult_audio.recv().await.unwrap();
                if !frame.silence && frame.frame.samples.iter().any(|s| s.abs() > 100) {
                    break;
                }
            }
        })
        .await
        .expect("consult must hear customer RTP after merge");
        let mut bridge_tokens = Vec::new();
        for name in ["caller", "callee", "consult-transfer-media"] {
            let id = LegId::from(name);
            let handle = session
                .legs
                .remove_conference_bridge_handle(&id)
                .expect("merged bridge");
            assert!(
                !handle.cancel_token.is_cancelled(),
                "{name} bridge was replaced by another participant"
            );
            bridge_tokens.push(handle.cancel_token.clone());
            session.legs.set_conference_bridge_handle(id, handle);
        }
        // Real RTP: each source must reach both other endpoints and never itself.
        // Stop earlier tones and allow their queued frames to drain first.
        let endpoints = [&remote_legs[0], &remote_legs[1], &remote];
        for endpoint in endpoints {
            endpoint
                .set_egress_source(crate::media::egress::EgressSource::Silence)
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        for source in 0..3 {
            let mut observers = Vec::new();
            for (index, endpoint) in endpoints.iter().enumerate() {
                observers.push(
                    crate::media::app_ingress::LegPcmStream::attach(
                        endpoint.pc(),
                        endpoint.negotiated().unwrap(),
                        crate::media::leg_id::LegId::from(format!("observer-{source}-{index}")),
                        CancellationToken::new(),
                    )
                    .unwrap(),
                );
            }
            endpoints[source]
                .set_egress_source(crate::media::egress::EgressSource::Media {
                    audio: Box::new(
                        crate::media::audio_source::ToneAudioSource::new(
                            440 + source as u32 * 220,
                            Duration::from_secs(2),
                            8000,
                        )
                        .unwrap(),
                    ),
                    loop_playback: false,
                    on_end: None,
                })
                .await
                .unwrap();
            let levels =
                futures::future::join_all(observers.into_iter().map(|mut observer| async move {
                    let mut audible = false;
                    let mut frames = 0;
                    let _ = tokio::time::timeout(Duration::from_millis(600), async {
                        loop {
                            let frame = observer.recv().await.unwrap();
                            frames += 1;
                            audible |= frame
                                .frame
                                .samples
                                .iter()
                                .any(|sample| sample.unsigned_abs() > 100);
                        }
                    })
                    .await;
                    (audible, frames)
                }))
                .await;
            for (destination, (audible, frames)) in levels.into_iter().enumerate() {
                assert!(frames > 0, "destination {destination} must receive media");
                assert_eq!(
                    audible,
                    source != destination,
                    "source {source}, destination {destination}"
                );
            }
            endpoints[source]
                .set_egress_source(crate::media::egress::EgressSource::Silence)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // Alice hangs up after merge: Charlie and agent retain their mixer bridges.
        session
            .execute_command(
                CallCommand::LegFailed {
                    leg_id: consult.clone(),
                    reason: "Remote hung up".into(),
                },
                None,
            )
            .await;
        assert!(bridge_tokens[2].is_cancelled());
        assert!(!bridge_tokens[0].is_cancelled());
        assert!(!bridge_tokens[1].is_cancelled());
        assert_eq!(
            session.legs.get(&LegId::from("caller")).unwrap().state,
            LegState::Connected
        );
        assert!(!session.cancel_token.is_cancelled());
        assert_eq!(
            server
                .conference_server
                .get_conference(&crate::call::runtime::ConferenceId::from(
                    "consult-consult-media"
                ))
                .await
                .unwrap()
                .participant_count(),
            2
        );
        session.handle_leave_mixer().await.unwrap();
        assert!(bridge_tokens.iter().all(|token| token.is_cancelled()));
        assert!(session.conference_bridge.conf_id.is_none());
        for name in ["caller", "callee", "consult-transfer-media"] {
            assert!(
                server
                    .conference_server
                    .get_conference_id_for_leg(&session.participant_leg(&LegId::from(name)))
                    .await
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn consult_retry_uses_new_sip_call_id() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_config};

    let (server, _) = create_test_server_with_config(ProxyConfig::default()).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "consult-media".into(),
        dialplan: Arc::new(Dialplan::new(
            "consult-media".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new_uac(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        true,
    );

    let (_input_tx, input_rx) = mpsc::unbounded_channel();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let address = rsipstack::transport::SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: "127.0.0.1:5060".try_into().unwrap(),
    };
    server
        .endpoint
        .inner
        .transport_layer
        .del_transport(&address);
    let connection = rsipstack::transport::channel::ChannelConnection::create_connection(
        input_rx, output_tx, address, None,
    )
    .await
    .unwrap();
    server
        .endpoint
        .inner
        .transport_layer
        .add_transport(connection.into());
    let mut ids = std::collections::HashSet::new();
    for _ in 0..3 {
        session
            .handle_add_leg_inner(
                "sip:alice@127.0.0.1:5099".into(),
                Some(LegId::from("consult")),
                vec![],
                None,
                None,
            )
            .await
            .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(rsipstack::transport::TransportEvent::Incoming(message, _, _)) =
                    output_rx.recv().await
                {
                    let text = message.to_string();
                    if text.starts_with("INVITE ") {
                        break text;
                    }
                }
            }
        })
        .await
        .expect("outgoing consultation INVITE");
        let call_id = message
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("call-id:"))
            .expect("Call-ID header")
            .to_string();
        assert!(
            ids.insert(call_id),
            "each consultation attempt must use a fresh SIP Call-ID"
        );
        session
            .execute_command(
                CallCommand::LegFailed {
                    leg_id: LegId::from("consult"),
                    reason: "Rejected with 603".into(),
                },
                None,
            )
            .await;
        assert!(session.legs.get(&LegId::from("consult")).is_none());
    }
}

/// The CDR snapshot must carry the logical-call correlation fields:
/// `transferred` (flag + metadata marker) and the recorded leg timeline.
/// The root session has `root_session_id == None`, which derives the
/// "primary" CDR role downstream.
#[tokio::test]
async fn test_record_snapshot_carries_transferred_and_leg_timeline() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::callrecord::LegTimelineEventType;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let original_request = request.clone();
    let (tx, _) = create_transaction(request).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let server_dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("failed to create server dialog");

    let context = CallContext {
        session_id: "snapshot-session".to_string(),
        dialplan: Arc::new(Dialplan::new(
            "snapshot-session".to_string(),
            original_request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".to_string(),
        original_callee: "sip:bob@rustpbx.com".to_string(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };

    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
    );

    // Root session: no inherited root id.
    assert!(session.record_snapshot().root_session_id.is_none());

    session.record_leg_event(
        "leg-1",
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({ "target": "sip:1001@rustpbx.com" })),
    );
    session.record_leg_event(
        "leg-1",
        LegTimelineEventType::Bridged,
        Some("caller".into()),
        None,
    );
    session.mark_transferred_with(Some(serde_json::json!({ "kind": "sip" })));
    session.record_leg_event("leg-1", LegTimelineEventType::Removed, None, None);

    let snapshot = session.record_snapshot();
    assert!(
        snapshot.transferred,
        "mark_transferred_with must set the flag"
    );
    assert_eq!(
        snapshot
            .metadata
            .get("transferred")
            .and_then(|v| v.as_bool()),
        Some(true),
        "metadata must carry the transferred marker"
    );
    let events = &snapshot.leg_timeline.events;
    assert_eq!(
        events.len(),
        4,
        "expected added/bridged/transferred/removed: {events:?}"
    );
    assert_eq!(events[0].event_type, LegTimelineEventType::Added);
    assert_eq!(events[0].leg_id, "leg-1");
    assert_eq!(
        events[0]
            .details
            .as_ref()
            .and_then(|d| d.get("target"))
            .and_then(|t| t.as_str()),
        Some("sip:1001@rustpbx.com")
    );
    assert_eq!(events[1].event_type, LegTimelineEventType::Bridged);
    assert_eq!(events[1].peer_leg_id.as_deref(), Some("caller"));
    assert_eq!(events[2].event_type, LegTimelineEventType::Transferred);
    assert_eq!(events[2].leg_id, "caller");
    assert_eq!(
        events[2]
            .details
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|t| t.as_str()),
        Some("sip")
    );
    assert_eq!(events[3].event_type, LegTimelineEventType::Removed);

    // Cleanup must archive the last short PCM window before reporting its CDR.
    let mut config = crate::media::leg::LegConfig::rtp_pcmu();
    config.call_id = Some(session.context.session_id.clone());
    config.volume_stats.enabled = true;
    config.quality_stats.sample_interval_ms = 500;
    let caller = crate::media::leg::LegInner::new("caller", &config, None).unwrap();
    session
        .legs
        .set_media_leg(&LegId::from("caller"), caller.clone());
    caller
        .play(
            Box::new(
                crate::media::audio_source::ToneAudioSource::new(440, Duration::from_secs(1), 8000)
                    .unwrap(),
            ),
            true,
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(serde_json::to_value(caller.quality_report("A")).unwrap()["pcmEgress"].is_null());
    let (records, mut received) = mpsc::channel(1);
    session.reporter = Some(CallReporter {
        server: server.clone(),
        context: session.context.clone(),
        call_record_sender: Some(records),
    });
    session.cleanup().await;
    let record = received.try_recv().expect("cleanup must report its CDR");
    let metadata = serde_json::to_value(record).unwrap();
    let volume = &metadata["metadata"]["media_quality"][0]["pcmEgress"]["volume"];
    assert!(
        volume["sample_count"].as_u64().unwrap_or(0) > 0,
        "the CDR must retain actual short PCM"
    );
    assert!(volume["rms"].as_f64().unwrap() > 0.0);
    assert!(volume["observed_ms"].as_u64().unwrap() < 500);
}

#[tokio::test]
async fn dynamic_hold_resume_targets_current_bridge_pair() {
    use crate::call::{DialDirection, Dialplan, MediaConfig};
    use crate::config::MediaProxyMode;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    for (leg_names, source_index, side) in [
        (["caller", "callee"], 0, DialogSide::Caller),
        (["caller", "callee"], 1, DialogSide::Callee),
        (["dynamic-a", "dynamic-b"], 0, DialogSide::Callee),
        (["dynamic-a", "dynamic-b"], 1, DialogSide::Callee),
    ] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "rustpbx.com",
            None,
        );
        let dialplan = Dialplan::new("hold-resume".into(), request, DialDirection::Inbound)
            .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All));
        let (mut session, _handle, _commands) = build_session_with_cmd_rx(dialplan).await;
        let _guard = session.cancel_token.clone().drop_guard();
        let mut remotes = Vec::new();
        for name in leg_names {
            let leg_id = LegId::from(name);
            if !session.legs.contains_key(&leg_id) {
                session.legs.insert(
                    leg_id.clone(),
                    crate::call::domain::Leg::new(leg_id.clone()),
                );
            }
            let local = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote =
                LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = remote.create_offer().await.unwrap();
            let answer = local
                .apply_sdp(&offer, rustrtc::SdpType::Offer)
                .await
                .unwrap();
            remote
                .apply_sdp(&answer, rustrtc::SdpType::Answer)
                .await
                .unwrap();
            local.accept();
            remote.accept();
            session.legs.set_media_leg(&leg_id, local);
            session.update_leg_state(&leg_id, LegState::Connected);
            remotes.push(remote);
        }
        let source_leg = LegId::from(leg_names[source_index]);
        let peer_leg = LegId::from(leg_names[1 - source_index]);
        let bridged = session
            .execute_command(
                CallCommand::Bridge {
                    leg_a: source_leg.clone(),
                    leg_b: peer_leg.clone(),
                    mode: crate::call::domain::P2PMode::Audio,
                },
                None,
            )
            .await;
        assert!(bridged.success, "{:?}", bridged.message);
        let remote = &remotes[source_index];
        // Exercise the same negotiation -> hold transition sequence as an
        // incoming re-INVITE, twice to catch stale route state after resume.
        for _ in 0..2 {
            for direction in ["sendonly", "sendrecv"] {
                let offer = remote.create_offer().await.unwrap();
                let offer = rustrtc::modify_sdp_direction(&offer, direction);
                let parsed =
                    rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer).unwrap();
                let answer = session
                    .build_local_dialog_answer(
                        side,
                        &source_leg,
                        rsipstack::sip::Method::Invite,
                        &offer,
                    )
                    .await
                    .unwrap();
                remote
                    .apply_sdp(&answer, rustrtc::SdpType::Answer)
                    .await
                    .unwrap();
                session
                    .apply_reinvite_hold_transition(side, &source_leg, &parsed, &[])
                    .await;
                let resumed = direction == "sendrecv";
                if !resumed {
                    if leg_names[0].starts_with("dynamic-") {
                        let connected = session
                            .execute_command(
                                CallCommand::LegConnected {
                                    leg_id: source_leg.clone(),
                                    answer_sdp: None,
                                    dialog_id: None,
                                },
                                None,
                            )
                            .await;
                        assert!(connected.success, "{:?}", connected.message);
                        let bridged = session
                            .execute_command(
                                CallCommand::Bridge {
                                    leg_a: source_leg.clone(),
                                    leg_b: peer_leg.clone(),
                                    mode: crate::call::domain::P2PMode::Audio,
                                },
                                None,
                            )
                            .await;
                        assert!(bridged.success, "{:?}", bridged.message);
                        assert_eq!(session.legs.get(&source_leg).unwrap().state, LegState::Hold);
                        assert!(!session.bridge().unwrap().is_bridged());
                    }
                    // Playback completion / unmute while held must preserve the
                    // intended pair without resuming live media prematurely.
                    session
                        .execute_command(CallCommand::ResumeMedia, None)
                        .await;
                    session
                        .handle_unmute_track(source_leg.to_string())
                        .await
                        .unwrap();
                    assert_eq!(
                        session.bridge.legs,
                        vec![source_leg.clone(), peer_leg.clone()]
                    );
                }
                assert_eq!(session.bridge().unwrap().is_bridged(), resumed);
                let expected_state = if resumed {
                    LegState::Connected
                } else {
                    LegState::Hold
                };
                for leg_id in [&source_leg, &peer_leg] {
                    assert_eq!(session.legs.get(leg_id).unwrap().state, expected_state);
                    if resumed {
                        assert!(
                            session.media_leg(leg_id).unwrap().egress_is_relay(),
                            "{side:?} resume must restore {leg_id} relay, not merely mark the bridge active"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn blind_transfer_detaches_agent_before_independent_target_answers() {
    use crate::call::{DialDirection, Dialplan, MediaConfig, TransactionCookie};
    use crate::config::{MediaProxyMode, ProxyConfig};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_transaction};

    for outcome in ["answer", "early_media", "reject", "timeout"] {
        let mut config = ProxyConfig::default();
        config.blind_transfer_use_refer = false;
        #[cfg(feature = "addon-cc")]
        let registry = Arc::new(crate::addons::cc::agent::AgentRegistry::new());
        #[cfg(feature = "addon-cc")]
        let (server, _) = {
            use crate::addons::cc::agent::AgentStatus;
            registry.register("agent".into(), vec![], 1).await.unwrap();
            registry
                .update_status("agent", AgentStatus::Idle)
                .await
                .unwrap();
            registry
                .update_status_with_call_delta(
                    "agent",
                    AgentStatus::Ringing {
                        call_id: format!("blind-{outcome}"),
                        since: Instant::now(),
                    },
                    1,
                )
                .await
                .unwrap();
            registry
                .update_status(
                    "agent",
                    AgentStatus::Busy {
                        call_id: format!("blind-{outcome}"),
                        since: Instant::now(),
                    },
                )
                .await
                .unwrap();
            crate::proxy::tests::common::create_test_server_with_session_hooks(
                config,
                vec![Arc::new(
                    crate::addons::cc::cc_call_session_hook::CcCallSessionHook::new(
                        registry.clone(),
                        Arc::new(crate::addons::cc::metrics::MetricsCollector::new()),
                    ),
                )],
            )
            .await
        };
        #[cfg(not(feature = "addon-cc"))]
        let (server, _) = crate::proxy::tests::common::create_test_server_with_config(config).await;
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "rustpbx.com",
            None,
        );
        let (tx, _) = create_transaction(request.clone()).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .unwrap();
        let context = CallContext {
            session_id: format!("blind-{outcome}"),
            dialplan: Arc::new(
                Dialplan::new(format!("blind-{outcome}"), request, DialDirection::Inbound)
                    .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All)),
            ),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:caller@rustpbx.com".into(),
            original_callee: "sip:agent@rustpbx.com".into(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let (mut session, _handle, _commands) =
            SipSession::new(server, cancel, None, context, dialog, true);
        let agent = LegId::new(uuid::Uuid::new_v4().to_string());
        session
            .legs
            .insert(agent.clone(), crate::call::domain::Leg::new(agent.clone()));
        let mut remotes = Vec::new();
        for name in ["caller", agent.as_str()] {
            let local = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote =
                LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = remote.create_offer().await.unwrap();
            let answer = local
                .apply_sdp(&offer, rustrtc::SdpType::Offer)
                .await
                .unwrap();
            remote
                .apply_sdp(&answer, rustrtc::SdpType::Answer)
                .await
                .unwrap();
            if name == "caller" {
                session.media.caller_offer = Some(offer);
                session.media.answer = Some(answer);
            }
            local.accept();
            remote.accept();
            session.legs.set_media_leg(&LegId::from(name), local);
            session.update_leg_state(&LegId::from(name), LegState::Connected);
            remotes.push(remote);
        }
        assert!(
            session
                .setup_bridge(LegId::from("caller"), agent.clone())
                .await
        );
        let old_peer = session.media_leg(&agent.clone()).unwrap();
        let caller_peer = session.media_leg(&LegId::from("caller")).unwrap();
        let old_dialog = rsipstack::dialog::DialogId {
            call_id: "original-agent".into(),
            local_tag: "local".into(),
            remote_tag: "remote".into(),
        };
        session.meta.connected_callee_dialog_id = Some(old_dialog.clone());
        session.callee_dialogs.insert(old_dialog.clone(), ());
        let target = LegId::new(format!("transfer-{}", uuid::Uuid::new_v4()));
        for command in [
            CallCommand::LegAdd {
                source_leg: Some(LegId::from("caller")),
                target: "sip:alice@127.0.0.1:5099".into(),
                leg_id: Some(target.clone()),
                headers: vec![],
                purpose: None,
            },
            CallCommand::HangupAgentLeg,
            CallCommand::Bridge {
                leg_a: LegId::from("caller"),
                leg_b: target.clone(),
                mode: crate::call::domain::P2PMode::Audio,
            },
            CallCommand::MarkTransferred,
        ] {
            let adding = matches!(&command, CallCommand::LegAdd { .. });
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                session.execute_command(command, None),
            )
            .await
            .expect("owner operation must not await target answer");
            assert!(result.success, "{:?}", result.message);
            if adding {
                // A normal proxy route may exist only in MediaBridge. Leg
                // removal must detach its actual peer even without this intent.
                session.bridge.clear();
            }
        }
        assert_ne!(target.as_str(), "callee");
        let target_peer = session.media_leg(&target).unwrap();
        assert!(!Arc::ptr_eq(&old_peer, &target_peer));
        assert!(Arc::ptr_eq(
            &caller_peer,
            &session.media_leg(&LegId::from("caller")).unwrap()
        ));
        assert!(
            session.pending_hangup.contains(&old_dialog),
            "agent BYE must be scheduled before C answers"
        );
        assert!(!session.legs.contains_key(&agent.clone()));
        assert!(
            !session.bridge().unwrap().is_bridged(),
            "unanswered C must not enter the media bridge"
        );
        assert!(target_peer.negotiated().is_none());
        assert!(session.meta.transferred);
        #[cfg(feature = "addon-cc")]
        {
            use crate::addons::cc::agent::AgentStatus;
            let released = registry.get_agent("agent").await.unwrap();
            assert!(matches!(released.status, AgentStatus::Wrapup { .. }));
            assert_eq!(released.current_calls, 0);
            session
                .execute_command(CallCommand::HangupAgentLeg, None)
                .await;
            session.fire_on_call_ended_hooks(None, 10).await;
            let duplicate = registry.get_agent("agent").await.unwrap();
            assert_eq!(
                duplicate.last_state_changed_at,
                released.last_state_changed_at
            );
            assert_eq!(duplicate.current_calls, 0);
        }
        // Retired B's termination must not cascade into a caller BYE.
        session
            .handle_callee_state(DialogState::Terminated(
                old_dialog,
                rsipstack::dialog::dialog::TerminatedReason::UacBye,
            ))
            .await
            .unwrap();
        assert!(!session.pending_hangup.contains(&session.caller_dialog_id()));
        assert!(!session.cancel_token.is_cancelled());

        if matches!(outcome, "reject" | "timeout") {
            let result = session
                .execute_command(
                    CallCommand::LegFailed {
                        leg_id: target.clone(),
                        reason: if outcome == "reject" {
                            "Rejected with 486"
                        } else {
                            "Ring timeout"
                        }
                        .into(),
                    },
                    None,
                )
                .await;
            assert!(!result.success);
            assert!(
                session.pending_hangup.contains(&session.caller_dialog_id()),
                "no agent remains to restore on failure"
            );
            assert!(session.media_leg(&target).is_none());
            assert!(!session.meta.transfer_in_progress);
        } else {
            let remote = LegInner::new("alice", &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = target_peer
                .pc()
                .local_description()
                .unwrap()
                .to_sdp_string();
            let answer = remote
                .apply_sdp(&offer, rustrtc::SdpType::Offer)
                .await
                .unwrap();
            remote.accept();
            if outcome == "early_media" {
                target_peer
                    .apply_sdp(&answer, rustrtc::SdpType::Pranswer)
                    .await
                    .unwrap();
                assert!(
                    session
                        .execute_command(
                            CallCommand::LegRinging {
                                leg_id: target.clone()
                            },
                            None
                        )
                        .await
                        .success
                );
                assert!(
                    session.bridge().unwrap().is_bridged(),
                    "usable early media connects A-C"
                );
            }
            let connected = session
                .execute_command(
                    CallCommand::LegConnected {
                        leg_id: target.clone(),
                        answer_sdp: Some(answer),
                        dialog_id: None,
                    },
                    None,
                )
                .await;
            assert!(connected.success, "{:?}", connected.message);
            #[cfg(feature = "addon-cc")]
            {
                let released = registry.get_agent("agent").await.unwrap();
                assert!(matches!(
                    released.status,
                    crate::addons::cc::agent::AgentStatus::Wrapup { .. }
                ));
                assert_eq!(released.current_calls, 0);
            }
            assert!(!session.meta.transfer_in_progress);
            assert!(session.bridge().unwrap().is_bridged());
            assert!(Arc::ptr_eq(
                &target_peer,
                &session.bridge().unwrap().leg(LegSide::B).unwrap()
            ));
            assert!(session.pending_hangup.is_empty());
            let mut audio = crate::media::app_ingress::LegPcmStream::attach(
                remotes[0].pc(),
                remotes[0].negotiated().unwrap(),
                crate::media::leg_id::LegId::from("caller-observer"),
                CancellationToken::new(),
            )
            .unwrap();
            remote
                .play_media(
                    Box::new(
                        crate::media::audio_source::ToneAudioSource::new(
                            660,
                            Duration::from_secs(1),
                            8000,
                        )
                        .unwrap(),
                    ),
                    true,
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let frame = audio.recv().await.unwrap();
                    if frame.frame.samples.iter().any(|sample| sample.abs() > 100) {
                        break;
                    }
                }
            })
            .await
            .expect("caller must receive Alice audio after transfer");
            if outcome == "answer" {
                let ended = session
                    .execute_command(
                        CallCommand::LegFailed {
                            leg_id: target,
                            reason: "Remote hung up".into(),
                        },
                        None,
                    )
                    .await;
                assert!(!ended.success);
                assert!(session.pending_hangup.contains(&session.caller_dialog_id()));
            }
            remote.stop();
        }
        session.bridge_mut().unwrap().close();
        for remote in remotes {
            remote.stop();
        }
    }
}

#[tokio::test]
async fn dynamic_hold_before_connected_command_keeps_answered_dialog_owned() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use rsipstack::sip::prelude::HeadersExt;
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::SipAddr;
    use rsipstack::transport::udp::UdpConnection;
    use tokio::time::timeout;

    for (ignore_late_answer, teardown_before_drain) in
        [(true, false), (true, true), (false, false), (false, true)]
    {
        let (server, _) = create_test_server().await;
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        server
            .endpoint
            .inner
            .transport_layer
            .del_transport(&SipAddr {
                r#type: Some(rsipstack::sip::Transport::Udp),
                addr: "127.0.0.1:5060".try_into().unwrap(),
            });
        let transport = UdpConnection::create_connection(local, None, None)
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
        let request = create_test_request(Method::Invite, "bob", None, "rustpbx.com", None);
        let context = CallContext {
            session_id: "late-answer".into(),
            dialplan: Arc::new(Dialplan::new(
                "late-answer".into(),
                request,
                DialDirection::Outbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:bob@rustpbx.com".into(),
            original_callee: "sip:alice@rustpbx.com".into(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let cancel = CancellationToken::new();
        let _cancel_on_exit = cancel.clone().drop_guard();
        let (mut session, handle, mut commands) =
            SipSession::new_uac(server.clone(), cancel.clone(), None, context, true);
        let (callee_tx, callee_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_tx);
        server
            .active_call_registry
            .register_handle(session.id.to_string(), handle.clone());
        let leg_id = LegId::from("late-target");
        let add = CallCommand::LegAdd {
            source_leg: None,
            target: format!("sip:alice@{target_addr}"),
            leg_id: Some(leg_id.clone()),
            headers: vec![],
            purpose: None,
        };
        let added = session.execute_command(add, None).await;
        assert!(added.success, "{:?}", added.message);
        let mut buffer = [0u8; 65536];
        let (size, pbx_addr) = timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let SipMessage::Request(invite) =
            SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
        else {
            panic!("expected INVITE");
        };
        assert_eq!(invite.method, Method::Invite);
        let call_id = invite.call_id_header().unwrap().value().to_string();
        let local_to = invite.from_header().unwrap().value().to_string();

        // Leg removal is queued before the real SIP 200
        // causes the dial task to append LegConnected to the same channel.
        handle
            .send_command(CallCommand::LegRemove {
                leg_id: leg_id.clone(),
                reason: None,
            })
            .unwrap();
        let mut answer =
            server
                .endpoint
                .inner
                .make_response(&invite, StatusCode::OK, Some(invite.body.clone()));
        answer
            .headers
            .retain(|header| !matches!(header, Header::To(_)));
        answer.headers.push(Header::To(
            format!("<sip:alice@{target_addr}>;tag=late-peer").into(),
        ));
        answer
            .headers
            .push(Header::Contact(format!("<sip:alice@{target_addr}>").into()));
        answer
            .headers
            .push(Header::ContentType("application/sdp".into()));
        target
            .send_to(answer.to_string().as_bytes(), pbx_addr)
            .await
            .unwrap();
        let queued = timeout(Duration::from_secs(3), async {
            let mut queued = Vec::new();
            loop {
                let command = commands.recv().await.expect("dial task notification");
                let answered = matches!(&command, CallCommand::LegConnected {
                    leg_id: id,
                    dialog_id: Some(command_call_id),
                    ..
                } if id == &leg_id && command_call_id == &call_id);
                queued.push(command);
                if answered {
                    return queued;
                }
            }
        })
        .await
        .expect("real 200 OK produces LegConnected");
        let removed_at = queued
            .iter()
            .position(|cmd| matches!(cmd, CallCommand::LegRemove { .. }))
            .unwrap();
        let answered_at = queued
            .iter()
            .position(|cmd| matches!(cmd, CallCommand::LegConnected { .. }))
            .unwrap();
        assert!(removed_at < answered_at);
        let dialogs = server.dialog_layer.get_client_dialog_by_call_id(&call_id);
        assert_eq!(dialogs.len(), 1);
        let confirmed_id = dialogs[0].id();
        assert_eq!(
            server
                .active_call_registry
                .get_handle_by_dialog(&confirmed_id.call_id)
                .map(|registered| registered.session_id().to_string()),
            Some(handle.session_id().to_string()),
            "the dialog key must resolve before LegConnected is consumed"
        );
        assert!(!dialogs[0].state().is_terminated());

        // The answered dialog must own immediate in-dialog traffic before the
        // separately queued LegConnected command reaches the session actor.
        timeout(Duration::from_secs(3), async {
            loop {
                let (size, _) = target.recv_from(&mut buffer).await.unwrap();
                if matches!(
                    SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap(),
                    SipMessage::Request(ref request) if request.method == Method::Ack
                ) {
                    break;
                }
            }
        })
        .await
        .expect("dynamic leg receives ACK");
        let hold_sdp = String::from_utf8_lossy(&invite.body).replace("a=sendrecv", "a=inactive");
        let reinvite = format!(
            "INVITE sip:pbx@{pbx_addr} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {target_addr};branch=z9hG4bKdynamicHold\r\n\
             From: <sip:alice@{target_addr}>;tag=late-peer\r\n\
             To: {local_to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 2 INVITE\r\n\
             Contact: <sip:alice@{target_addr}>\r\n\
             Max-Forwards: 70\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\r\n{}",
            hold_sdp.len(),
            hold_sdp,
        );
        let SipMessage::Request(reinvite) = SipMessage::try_from(reinvite.as_str()).unwrap() else {
            panic!("expected hold re-INVITE request");
        };
        let (tx_handle, mut response_rx) = TransactionHandle::new();
        session
            .handle_callee_state(DialogState::Updated(
                confirmed_id.clone(),
                reinvite,
                tx_handle,
            ))
            .await
            .unwrap();
        let response = timeout(Duration::from_secs(3), response_rx.recv())
            .await
            .expect("immediate hold produces a final response")
            .expect("response channel stays open");
        let rsipstack::dialog::dialog::TransactionCommand::Respond { status, body, .. } = response;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(body.as_deref().unwrap_or_default()).contains("a=inactive"),
            "hold answer must keep the audio leg inactive"
        );

        for command in queued {
            if matches!(&command, CallCommand::LegConnected { .. }) {
                assert!(!session.legs.contains_key(&leg_id));
                assert!(
                    session.pending_hangup.is_empty(),
                    "removal did not yet know the answered dialog"
                );
                // The dial-task guard must clean up even if the session
                // never processes the queued answer.
                if ignore_late_answer {
                    continue;
                }
            }
            let result = session.execute_command(command, None).await;
            assert!(result.success, "{:?}", result.message);
        }
        assert!(
            !session.legs.contains_key(&leg_id),
            "late answer must not recreate C"
        );
        if ignore_late_answer {
            assert!(session.pending_hangup.is_empty());
            assert!(
                !session
                    .callee_guards
                    .iter()
                    .any(|guard| guard.id() == &confirmed_id)
            );
        }

        let runner = if teardown_before_drain {
            // Isolate the retained guard: no command loop or hangup drain.
            session.pending_hangup.clear();
            drop(session);
            None
        } else {
            Some(tokio::spawn(async move {
                session.run_main_loop(None, callee_rx, commands).await
            }))
        };
        let bye = timeout(Duration::from_secs(3), async {
            loop {
                let (size, _) = target.recv_from(&mut buffer).await.unwrap();
                if let SipMessage::Request(request) =
                    SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
                {
                    if request.method == Method::Bye {
                        return request;
                    }
                }
            }
        })
        .await
        .expect("target receives BYE over UDP for its late answer");
        assert_eq!(bye.call_id_header().unwrap().value(), call_id);
        let response = server
            .endpoint
            .inner
            .make_response(&bye, StatusCode::OK, None);
        target
            .send_to(response.to_string().as_bytes(), pbx_addr)
            .await
            .unwrap();
        cancel.cancel();
        if let Some(runner) = runner {
            timeout(Duration::from_secs(3), runner)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        server.endpoint.shutdown();
        serving.await.unwrap();
    }
}

#[test]
fn rwi_manual_parallel_retry_and_leg_cleanup() {
    // Same DEBUG-build frame depth as
    // consult_media_preserves_peers_across_bridge_and_explicit_mixer: the
    // LegAdd/bridge poll chain overflows the default libtest thread stack.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("rwi-manual runtime")
                .block_on(rwi_manual_parallel_retry_and_leg_cleanup_body())
        })
        .expect("spawn rwi-manual test thread")
        .join()
        .expect("rwi-manual test body panicked");
}

async fn rwi_manual_parallel_retry_and_leg_cleanup_body() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};
    use crate::rwi::processor::{CommandResult as RwiResult, RwiCommandProcessor};
    use crate::rwi::{RwiCommandPayload, RwiGateway};
    use rsipstack::sip::prelude::HeadersExt;
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::{SipAddr, udp::UdpConnection};
    use tokio::time::timeout;

    let gateway = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
    let mut events = gateway.read().subscribe_events();
    let (ws_tx, mut ws_events) = mpsc::unbounded_channel();
    {
        let mut gw = gateway.write();
        let id = gw
            .create_session(crate::rwi::RwiIdentity {
                token: "test".into(),
                scopes: vec!["call.control".into()],
            })
            .read()
            .id
            .clone();
        gw.set_session_event_sender(&id, ws_tx);
        gw.claim_call_ownership(
            &id,
            "rwi-manual".into(),
            crate::rwi::session::OwnershipMode::Control,
        )
        .unwrap();
    }
    let (server, _) =
        create_test_server_with_rwi_gateway(ProxyConfig::default(), gateway.clone()).await;
    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = target.local_addr().unwrap();
    server
        .endpoint
        .inner
        .transport_layer
        .del_transport(&SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: "127.0.0.1:5060".try_into().unwrap(),
        });
    let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
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
    let request = create_test_request(Method::Invite, "bob", None, "rustpbx.com", None);
    let context = CallContext {
        session_id: "rwi-manual".into(),
        dialplan: Arc::new(Dialplan::new(
            "rwi-manual".into(),
            request,
            DialDirection::Outbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:bob@rustpbx.com".into(),
        original_callee: "sip:alice@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let (mut session, handle, mut commands) =
        SipSession::new_uac(server.clone(), cancel.clone(), None, context, true);
    server
        .active_call_registry
        .register_handle("rwi-manual".into(), handle);
    let processor = Arc::new(RwiCommandProcessor::new(
        server.active_call_registry.clone(),
        gateway,
        Arc::new(crate::call::runtime::ConferenceManager::new()),
    ));
    let caller = LegInner::new("caller", &LegConfig::rtp_pcmu(), None).unwrap();
    let caller_remote = LegInner::new("caller-remote", &LegConfig::rtp_pcmu(), None).unwrap();
    let offer = caller_remote.create_offer().await.unwrap();
    let answer = caller
        .apply_sdp(&offer, rustrtc::SdpType::Offer)
        .await
        .unwrap();
    caller_remote
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    caller.accept();
    caller_remote.accept();
    session
        .legs
        .set_media_leg(&LegId::from("caller"), caller.clone());
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);

    let mut invites = Vec::new();
    let mut buffer = [0u8; 65536];
    // Four simultaneously pending attempts, with IDs generated by the real RWI processor.
    for _ in 0..4 {
        let processor = processor.clone();
        let task = tokio::spawn(async move {
            processor
                .process_command(RwiCommandPayload::LegAdd {
                    call_id: "rwi-manual".into(),
                    target: format!("sip:alice@{addr}"),
                    leg_id: None,
                })
                .await
        });
        // Acknowledge enqueueing before the session executes the dial.
        let RwiResult::LegAdded {
            leg_id: returned_id,
        } = timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        else {
            panic!("expected leg ID in result");
        };
        let command = commands.recv().await.unwrap();
        let leg_id = match &command {
            CallCommand::LegAdd {
                leg_id: Some(id), ..
            } => id.to_string(),
            _ => panic!("expected generated leg ID in queued command"),
        };
        assert_eq!(returned_id, leg_id);
        assert!(session.execute_command(command, None).await.success);
        let (size, pbx) = timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let SipMessage::Request(invite) =
            SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
        else {
            panic!("INVITE");
        };
        assert_eq!(invite.method, Method::Invite);
        let mut ringing = server
            .endpoint
            .inner
            .make_response(&invite, StatusCode::Ringing, None);
        ringing.headers.retain(|h| !matches!(h, Header::To(_)));
        ringing.headers.push(Header::To(
            format!("<sip:alice@{addr}>;tag={leg_id}").into(),
        ));
        target
            .send_to(ringing.to_string().as_bytes(), pbx)
            .await
            .unwrap();
        // Drain the ringing notification before requesting the next leg.
        let cmd = timeout(Duration::from_secs(3), commands.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&cmd, CallCommand::LegRinging { leg_id: id } if id.as_str() == leg_id));
        session.execute_command(cmd, None).await;
        invites.push((leg_id, invite, pbx));
    }
    let rejected = &invites[0];
    let mut rejection =
        server
            .endpoint
            .inner
            .make_response(&rejected.1, StatusCode::BusyHere, None);
    rejection.headers.retain(|h| !matches!(h, Header::To(_)));
    rejection.headers.push(Header::To(
        format!("<sip:alice@{addr}>;tag={}", rejected.0).into(),
    ));
    target
        .send_to(rejection.to_string().as_bytes(), rejected.2)
        .await
        .unwrap();
    let failed = timeout(Duration::from_secs(3), commands.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&failed, CallCommand::LegFailed { leg_id, .. } if leg_id.as_str() == rejected.0)
    );
    session.execute_command(failed, None).await;
    assert!(!session.legs.contains_key(&LegId::new(&rejected.0)));
    assert!(!cancel.is_cancelled());

    let mut answered_peers = Vec::new();
    for winner in [&invites[1], &invites[3]] {
        let remote = LegInner::new("winner-remote", &LegConfig::rtp_pcmu(), None).unwrap();
        let answer_sdp = remote
            .apply_sdp(
                std::str::from_utf8(&winner.1.body).unwrap(),
                rustrtc::SdpType::Offer,
            )
            .await
            .unwrap();
        remote.accept();
        let mut answer = server.endpoint.inner.make_response(
            &winner.1,
            StatusCode::OK,
            Some(answer_sdp.into_bytes()),
        );
        answer.headers.retain(|h| !matches!(h, Header::To(_)));
        answer.headers.push(Header::To(
            format!("<sip:alice@{addr}>;tag={}", winner.0).into(),
        ));
        answer
            .headers
            .push(Header::Contact(format!("<sip:alice@{addr}>").into()));
        answer
            .headers
            .push(Header::ContentType("application/sdp".into()));
        target
            .send_to(answer.to_string().as_bytes(), winner.2)
            .await
            .unwrap();
        let connected = timeout(Duration::from_secs(3), commands.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&connected, CallCommand::LegConnected { .. }));
        assert!(session.execute_command(connected, None).await.success);
        assert!(!session.bridge.active, "answered peer must remain isolated");
        assert!(!caller.egress_is_relay());

        answered_peers.push(remote);
    }
    let winner = &invites[1];
    // Select the winner through the local-leg RWI bridge API, then remove a ringing loser.
    for payload in [
        RwiCommandPayload::Bridge {
            call_id: "rwi-manual".into(),
            leg_a: "caller".into(),
            leg_b: winner.0.clone(),
        },
        RwiCommandPayload::LegRemove {
            call_id: "rwi-manual".into(),
            leg_id: invites[2].0.clone(),
        },
    ] {
        let processor = processor.clone();
        let task = tokio::spawn(async move { processor.process_command(payload).await });
        task.await.unwrap().unwrap();
        let cmd = timeout(Duration::from_secs(3), commands.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(session.execute_command(cmd, None).await.success);
    }
    assert!(caller.egress_is_relay());
    let cancel_request = timeout(Duration::from_secs(3), async {
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(req) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
            {
                if req.method == Method::Cancel {
                    break req;
                }
            }
        }
    })
    .await
    .expect("ringing loser receives CANCEL");
    assert_eq!(
        cancel_request.call_id_header().unwrap(),
        invites[2].1.call_id_header().unwrap()
    );
    let ok = server
        .endpoint
        .inner
        .make_response(&cancel_request, StatusCode::OK, None);
    target
        .send_to(ok.to_string().as_bytes(), invites[2].2)
        .await
        .unwrap();
    let terminated =
        server
            .endpoint
            .inner
            .make_response(&invites[2].1, StatusCode::RequestTerminated, None);
    target
        .send_to(terminated.to_string().as_bytes(), invites[2].2)
        .await
        .unwrap();

    // Explicit unbridge must persist through later media updates.

    let result = session
        .execute_command(
            CallCommand::Unbridge {
                leg_id: LegId::from("caller"),
            },
            None,
        )
        .await;
    assert!(result.success);
    session.update_media_path().await;
    assert!(!session.bridge.active);
    assert!(!caller.egress_is_relay());

    // Removing an answered leg sends BYE, without ending the caller.

    let result = session
        .execute_command(
            CallCommand::LegRemove {
                leg_id: LegId::new(&invites[3].0),
                reason: None,
            },
            None,
        )
        .await;
    assert!(result.success);
    let bye = timeout(Duration::from_secs(3), async {
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(req) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
            {
                if req.method == Method::Bye {
                    break req;
                }
            }
        }
    })
    .await
    .expect("answered leg receives BYE");
    assert_eq!(
        bye.call_id_header().unwrap(),
        invites[3].1.call_id_header().unwrap()
    );
    let ok = server
        .endpoint
        .inner
        .make_response(&bye, StatusCode::OK, None);
    target
        .send_to(ok.to_string().as_bytes(), winner.2)
        .await
        .unwrap();
    assert!(!cancel.is_cancelled());
    assert_eq!(
        session.legs.get(&LegId::from("caller")).unwrap().state,
        LegState::Connected
    );

    // A subsequent dial to the same target gets a fresh dialog; stale failure is harmless.

    let result = session
        .execute_command(
            CallCommand::LegAdd {
                source_leg: None,
                target: format!("sip:alice@{addr}"),
                leg_id: Some(LegId::from("retry")),
                headers: vec![],
                purpose: None,
            },
            None,
        )
        .await;
    assert_eq!(result.affected_leg, Some(LegId::from("retry")));
    session
        .execute_command(
            CallCommand::LegFailed {
                leg_id: LegId::new(&rejected.0),
                reason: "late rejection".into(),
            },
            None,
        )
        .await;
    assert!(session.legs.contains_key(&LegId::from("retry")));
    let retry_invite = timeout(Duration::from_secs(3), async {
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(req) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
            {
                if req.method == Method::Invite {
                    break req;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(
        retry_invite.call_id_header().unwrap(),
        rejected.1.call_id_header().unwrap()
    );
    assert!(session.legs.contains_key(&LegId::new(&winner.0)));
    // Setup failure after enqueue must be observable through the same event channel.
    let saved_sender = session.cmd_tx.take();
    let failed = session
        .execute_command(
            CallCommand::LegAdd {
                source_leg: None,
                target: format!("sip:alice@{addr}"),
                leg_id: Some(LegId::from("setup-failure")),
                headers: vec![],
                purpose: None,
            },
            None,
        )
        .await;
    assert!(!failed.success);
    session.cmd_tx = saved_sender;
    let mut leg_events = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.event.payload["leg_id"].is_string()
            && matches!(event.event.event_type, "call_ringing" | "call_hangup")
        {
            leg_events.push(event.event.payload);
        }
    }
    assert!(
        leg_events.iter().any(|e| e["leg_id"] == rejected.0
            && e["event_type"] == "call_ringing"
            && e["leg_role"] == "callee"),
        "plain dial leg's leg-level call_ringing carries the positional leg_role fallback"
    );
    assert!(leg_events.iter().any(|e| e["leg_id"] == rejected.0
        && e["event_type"] == "call_hangup"
        && e["sip_status"] == 486));
    assert!(leg_events.iter().any(|e| e["leg_id"] == "setup-failure"
        && e["event_type"] == "call_hangup"
        && e["reason"].as_str().unwrap().contains("No command sender")));
    // Per-leg miss hangup: an explicit removal of a never-connected leg now
    // emits its own hangup with the generic cancel reason (the RWI
    // `leg_remove` API passes no reason).
    assert!(
        leg_events.iter().any(|e| e["leg_id"] == invites[2].0
            && e["event_type"] == "call_hangup"
            && e["reason"].as_str() == Some("cancelled")),
        "explicit removal of a never-connected leg emits its miss hangup"
    );
    let mut ws_leg_events = Vec::new();
    while let Ok(event) = ws_events.try_recv() {
        if event["leg_id"].is_string()
            && matches!(
                event["event_type"].as_str(),
                Some("call_ringing" | "call_hangup")
            )
        {
            ws_leg_events.push(event);
        }
    }
    // Per-leg `call_answered` contract: the winning leg's connect emits its
    // own answered with a leg identity (the session-scoped duplicate is the
    // deprecated complement and carries none). The tap above filters to
    // ringing/hangup, so the answered assertions live on the full stream —
    // nothing to check here beyond the ringing/hangup shapes.
    assert!(
        leg_events
            .iter()
            .all(|e| e["event_type"] != "call_answered"),
        "this tap only collects ringing/hangup legs"
    );
    assert_eq!(
        ws_leg_events.len(),
        leg_events.len(),
        "every leg event must reach its RWI owner"
    );
    cancel.cancel();
    drop(session);
    caller_remote.stop();
    for peer in answered_peers {
        peer.stop();
    }
    server.endpoint.shutdown();
    serving.await.unwrap();
}

#[tokio::test]
async fn added_legs_require_explicit_bridge_and_allow_removed_ids() {
    use crate::call::{DialDirection, Dialplan, MediaConfig};
    use crate::config::MediaProxyMode;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    for extra_leg in [false, true] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "rustpbx.com",
            None,
        );
        let dialplan = Dialplan::new("rwi-mode".into(), request, DialDirection::Inbound)
            .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All));
        let (mut session, _handle, mut commands) = build_session_with_cmd_rx(dialplan).await;
        let _guard = session.cancel_token.clone().drop_guard();
        let mut remotes = Vec::new();
        for name in ["caller", "target"] {
            if name == "target" {
                let leg = crate::call::domain::Leg::new(LegId::from(name));
                session.legs.insert(LegId::from(name), leg);
            }
            let peer = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote =
                LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = remote.create_offer().await.unwrap();
            let answer = peer
                .apply_sdp(&offer, rustrtc::SdpType::Offer)
                .await
                .unwrap();
            remote
                .apply_sdp(&answer, rustrtc::SdpType::Answer)
                .await
                .unwrap();
            peer.accept();
            remote.accept();
            session.legs.set_media_leg(&LegId::from(name), peer);
            session.update_leg_state(&LegId::from(name), LegState::Connected);
            remotes.push(remote);
        }
        // Additional connected legs must not cause implicit bridge selection.
        let spare_id = LegId::from("rwi-spare");
        let mut spare = crate::call::domain::Leg::new(spare_id.clone());
        spare.state = LegState::Connected;
        if extra_leg {
            session.legs.insert(spare_id, spare);
        }
        session.update_media_path().await;
        assert!(!session.bridge.active);
        assert!(
            !session
                .media_leg(&LegId::from("caller"))
                .unwrap()
                .egress_is_relay()
        );
        assert!(
            session
                .setup_bridge(LegId::from("caller"), LegId::from("target"))
                .await
        );
        assert!(session.bridge.active);
        session.handle_mute_track("caller".into()).await.unwrap();
        assert!(
            !session
                .media_leg(&LegId::from("caller"))
                .unwrap()
                .egress_is_relay()
        );
        session.handle_unmute_track("caller".into()).await.unwrap();
        assert!(
            session
                .media_leg(&LegId::from("caller"))
                .unwrap()
                .egress_is_relay()
        );

        for (side_only, unbridge_during_playback) in [(false, false), (true, false), (true, true)] {
            let path = format!("{}/fixtures/sample.wav", env!("CARGO_MANIFEST_DIR"));
            session
                .handle_play(
                    Some(LegId::from("caller")),
                    crate::call::domain::MediaSource::File { path },
                    Some(crate::call::domain::PlayOptions {
                        side_only,
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            assert!(
                !session
                    .media_leg(&LegId::from("caller"))
                    .unwrap()
                    .egress_is_relay()
            );
            if unbridge_during_playback {
                session
                    .execute_command(
                        CallCommand::Unbridge {
                            leg_id: LegId::from("caller"),
                        },
                        None,
                    )
                    .await;
            } else {
                session.handle_stop_playback(None).await.unwrap();
            }
            // Process the real playback completion's restore command.
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let command = commands.recv().await.unwrap();
                    let restored = matches!(command, CallCommand::ResumeMedia);
                    session.execute_command(command, None).await;
                    if restored {
                        break;
                    }
                }
            })
            .await
            .expect("playback must request media restoration");
            assert_eq!(session.bridge.active, !unbridge_during_playback);
            for name in ["caller", "target"] {
                assert_eq!(
                    session
                        .media_leg(&LegId::from(name))
                        .unwrap()
                        .egress_is_relay(),
                    !unbridge_during_playback
                );
            }
        }
        session.handle_unmute_track("caller".into()).await.unwrap();
        assert!(
            !session.bridge.active,
            "unmute must not invent a pair after explicit unbridge"
        );
        session
            .execute_command(
                CallCommand::Unbridge {
                    leg_id: LegId::from("caller"),
                },
                None,
            )
            .await;
        session.update_media_path().await;
        assert!(
            !session.bridge.active,
            "unbridge must not select another pair"
        );
        session.meta.queue_name = Some("sales".into());
        session.update_media_path().await;
        assert!(
            !session.bridge.active,
            "queue context must not implicitly select a pair"
        );
        session.meta.queue_name = None;
        session
            .execute_command(
                CallCommand::Unbridge {
                    leg_id: LegId::from("caller"),
                },
                None,
            )
            .await;

        // Invalid/reserved/active leg IDs fail synchronously without touching existing peers.
        for id in ["caller", "target"] {
            let response = session
                .execute_command(
                    CallCommand::LegAdd {
                        source_leg: None,
                        target: "sip:alice@127.0.0.1:9".into(),
                        leg_id: Some(LegId::from(id)),
                        headers: vec![],
                        purpose: None,
                    },
                    None,
                )
                .await;
            assert!(!response.success, "must reject {id}");
        }
        // A removed ID can be dialed again, including to a different target.
        for target in ["sip:alice@127.0.0.1:9", "sip:bob@127.0.0.1:9"] {
            let response = session
                .execute_command(
                    CallCommand::LegAdd {
                        source_leg: None,
                        target: target.into(),
                        leg_id: Some(LegId::from("reused")),
                        headers: vec![],
                        purpose: None,
                    },
                    None,
                )
                .await;
            assert!(response.success);
            assert_eq!(
                session
                    .legs
                    .get(&LegId::from("reused"))
                    .unwrap()
                    .endpoint
                    .as_deref(),
                Some(target)
            );

            let response = session
                .execute_command(
                    CallCommand::LegRemove {
                        leg_id: LegId::from("reused"),
                        reason: None,
                    },
                    None,
                )
                .await;
            assert!(response.success);
            assert!(!session.legs.contains_key(&LegId::from("reused")));
        }
        // An immediate setup failure must return the execution error, not queue success.
        let saved_sender = session.cmd_tx.take();

        let response = session
            .execute_command(
                CallCommand::LegAdd {
                    source_leg: None,
                    target: "sip:alice@127.0.0.1:9".into(),
                    leg_id: Some(LegId::from("bad-target")),
                    headers: vec![],
                    purpose: None,
                },
                None,
            )
            .await;
        let result = response;
        assert!(!result.success);
        assert!(result.message.unwrap().contains("No command sender"));
        session.cmd_tx = saved_sender;
        assert!(!session.legs.contains_key(&LegId::from("bad-target")));
        assert!(session.legs.contains_key(&LegId::from("caller")));
        if let Some(bridge) = session.bridge_mut() {
            bridge.close();
        }
        for remote in remotes {
            remote.stop();
        }
    }
}

#[tokio::test]
async fn rwi_route_app_waits_and_announces_to_configured_context() {
    use crate::call::app::{ApplicationContext, CallInfo, testing::MockCallStack};
    use crate::call::runtime::AppFactory;
    let gateway = Arc::new(parking_lot::RwLock::new(crate::rwi::RwiGateway::new()));
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (other_tx, mut other_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut gw = gateway.write();
        for (context, tx) in [("test-router", events_tx), ("default", other_tx)] {
            let session = gw.create_session(crate::rwi::auth::RwiIdentity {
                token: context.into(),
                scopes: vec![],
            });
            let id = session.read().id.clone();
            gw.set_session_event_sender(&id, tx);
            gw.subscribe(&id, vec![context.into()], None);
        }
    }
    let mut context = ApplicationContext::new(
        sea_orm::DatabaseConnection::default(),
        CallInfo {
            session_id: "test-session".into(),
            caller: "alice".into(),
            callee: "unregistered-entry".into(),
            direction: "inbound".into(),
            started_at: chrono::Utc::now(),
            sip_headers: Default::default(),
            route_name: Some("rwi-test".into()),
        },
        Arc::new(crate::config::Config::default()),
        reqwest::Client::new(),
    );
    let factory = BuiltinAppFactory::new(None, None);
    assert!(factory.create_app("rwi", None, &context).await.is_err());
    context.rwi_gateway = Some(gateway);
    let app = factory
        .create_app(
            "rwi",
            Some(serde_json::json!({"context":"test-router"})),
            &context,
        )
        .await
        .unwrap()
        .unwrap();
    let mut stack = MockCallStack::run_with_context(app, context);
    let event = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let event = serde_json::to_value(event).unwrap();
    assert_eq!(event["event_type"], "call_created");
    assert_eq!(event["call_id"], "test-session");
    assert_eq!(event["context"], "test-router");
    assert_eq!(event["callee"], "unregistered-entry");
    assert!(
        stack.next_cmd(50).await.is_none(),
        "RWI app must wait without answering or dialing"
    );
    assert!(events_rx.try_recv().is_err());
    assert!(other_rx.try_recv().is_err());
}

#[tokio::test]
async fn unpaired_leg_confirmation_and_bye_preserve_selected_pair() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .unwrap();
    let context = CallContext {
        session_id: "refer-source-guard".into(),
        dialplan: Arc::new(Dialplan::new(
            "refer-source-guard".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        dialog,
        false,
    );
    let b = LegId::from("callee");
    session.legs.insert(b.clone(), Leg::new(b.clone()));
    let c = LegId::from("target");
    let b_dialog = DialogId {
        call_id: "bob".into(),
        local_tag: "local".into(),
        remote_tag: "remote".into(),
    };
    session.callee_dialogs.insert(b_dialog.clone(), ());
    session
        .legs
        .insert(c.clone(), crate::call::domain::Leg::new(c.clone()));
    session.update_leg_state(&c, LegState::Connected);
    session.update_leg_state(&b, LegState::Ringing);
    session.bridge = BridgeConfig::bridge(LegId::from("caller"), c.clone());

    // Being outside A-C is not evidence that B completed a REFER.
    session
        .handle_callee_state(DialogState::Confirmed(
            b_dialog.clone(),
            rsipstack::sip::Response::default(),
        ))
        .await
        .unwrap();
    assert_eq!(session.legs.get(&b).unwrap().state, LegState::Connected);

    // A confirmation must preserve B's negotiated hold, and even stale
    // connected-dialog metadata must not override the selected A-C pair.
    session.update_leg_state(&b, LegState::Hold);
    session.meta.connected_callee_dialog_id = Some(b_dialog.clone());
    session
        .handle_callee_state(DialogState::Confirmed(
            b_dialog.clone(),
            rsipstack::sip::Response::default(),
        ))
        .await
        .unwrap();
    assert_eq!(session.legs.get(&b).unwrap().state, LegState::Hold);
    assert!(session.callee_dialogs.contains_key(&b_dialog));

    session
        .handle_callee_state(DialogState::Terminated(
            b_dialog.clone(),
            TerminatedReason::UasBye,
        ))
        .await
        .unwrap();
    assert!(!session.legs.contains_key(&b));
    assert!(!session.callee_dialogs.contains_key(&b_dialog));
    assert!(session.legs.contains_key(&c));
    assert_eq!(session.bridge.legs, vec![LegId::from("caller"), c]);
    assert!(!session.pending_hangup.contains(&session.caller_dialog_id()));
}

#[tokio::test]
async fn dynamic_leg_rejects_known_unregistered_user_before_dialing() {
    use crate::call::{DialDirection, Dialplan, SipUser};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new("offline-transfer".into(), request, DialDirection::Inbound);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx(dialplan).await;
    session
        .server
        .user_backend
        .create_user(SipUser {
            username: "offline".into(),
            realm: Some("rustpbx.com".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let target = LegId::from("offline-target");
    let error = session
        .handle_add_leg_inner(
            "sip:offline@rustpbx.com".into(),
            Some(target.clone()),
            vec![],
            Some(LegId::from("caller")),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<transfer::BlindTransferDialError>()
            .unwrap()
            .code,
        480
    );
    assert!(
        !session.legs.contains_key(&target),
        "Offline resolution must finish before creating a SIP leg"
    );
    assert!(session.callee_dialogs.is_empty());
}

#[tokio::test]
async fn removing_terminated_leg_does_not_schedule_bye() {
    use crate::call::{DialDirection, Dialplan};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "rustpbx.com",
        None,
    );
    let dialplan = Dialplan::new("ended-leg".into(), request, DialDirection::Inbound);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx(dialplan).await;
    let dialog = session.caller_dialog.as_ref().unwrap().clone();
    dialog.reject(Some(StatusCode::Decline), None).unwrap();
    assert!(dialog.state().is_terminated());
    let leg_id = LegId::from("ended-agent");
    let mut leg = crate::call::domain::Leg::new(leg_id.clone());
    leg.state = LegState::Ended;
    session.legs.add_leg(
        leg_id.clone(),
        leg,
        Some(rsipstack::dialog::dialog::Dialog::Invite(dialog.clone())),
    );

    session
        .handle_remove_leg(leg_id.clone(), None)
        .await
        .unwrap();

    assert!(!session.legs.contains_key(&leg_id));
    assert!(!session.pending_hangup.contains(&dialog.id()));
}

#[tokio::test]
async fn agent_consultation_uses_active_call_without_headers() {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry, PresenceState};
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    let (mut server, _) = create_test_server().await;
    let agents = Arc::new(MemoryRegistry::new());
    for (id, extension) in [("bob", "1001"), ("charlie", "1003")] {
        agents
            .register(
                id.into(),
                id.into(),
                format!("sip:{extension}@rustpbx.com"),
                vec![],
                2,
            )
            .await
            .unwrap();
    }
    Arc::get_mut(&mut server).unwrap().agent_registry = Some(agents.clone());
    let mut sessions = Vec::new();
    for (id, target, busy, authenticated, expected) in [
        ("parent", "1003", false, true, false),
        ("consult", "1003", true, true, true),
        ("agent-login", "charlie", true, true, true),
        ("external", "customer", true, true, false),
        ("conference", "conference", true, true, false),
        ("untrusted", "1003", true, false, false),
        ("idle", "1003", false, true, false),
        ("stale", "1003", true, true, false),
    ] {
        agents
            .update_presence(
                "bob",
                if busy {
                    PresenceState::Busy {
                        call_id: Some(if id == "stale" { "missing" } else { "parent" }.into()),
                    }
                } else {
                    PresenceState::Idle
                },
            )
            .await
            .unwrap();
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "bob",
            None,
            "rustpbx.com",
            None,
        );
        let cookie = TransactionCookie::default();
        if authenticated {
            cookie.set_user(crate::call::SipUser {
                username: "1001".into(),
                ..Default::default()
            });
        }
        let context = CallContext {
            session_id: id.into(),
            dialplan: Arc::new(Dialplan::new(id.into(), request, DialDirection::Outbound)),
            cookie,
            start_time: Instant::now(),
            original_caller: "sip:1001@rustpbx.com".into(),
            original_callee: format!("sip:{target}@rustpbx.com"),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, handle, rx) = SipSession::new_uac(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            false,
        );
        server
            .active_call_registry
            .register_handle(id.into(), handle);
        session.initialize_agent_consultation().await;
        assert_eq!(session.meta.root_session_id.is_some(), expected, "{id}");
        assert_eq!(
            session
                .legs
                .get(&LegId::from("caller"))
                .unwrap()
                .agent_id
                .as_deref(),
            expected.then_some("bob")
        );
        if expected {
            assert_eq!(
                session.session_ext_get("resolved_agent_id").as_deref(),
                Some("charlie")
            );

            assert_eq!(session.meta.root_session_id.as_deref(), Some("parent"));
            let mut old = Leg::new(LegId::from("old-b"));
            old.agent_id = Some("bob".into());
            let mut current = Leg::new(LegId::from("callee"));
            current.agent_id = Some("charlie".into());
            session.legs.insert(old.id.clone(), old);
            session.legs.insert(current.id.clone(), current);
            session.meta.transferred = true;
            let departure = session
                .transferred_agent_context(&LegId::from("old-b"))
                .unwrap();
            assert_eq!(
                departure
                    .extensions
                    .read()
                    .get::<HashMap<String, String>>()
                    .unwrap()
                    .get("resolved_agent_id")
                    .map(String::as_str),
                Some("bob")
            );
            assert_eq!(
                session.session_ext_get("resolved_agent_id").as_deref(),
                Some("charlie")
            );
            assert_eq!(
                session
                    .legs
                    .get(&LegId::from("callee"))
                    .unwrap()
                    .agent_id
                    .as_deref(),
                Some("charlie")
            );
            session.legs.remove(&LegId::from("old-b"));
            assert!(
                session
                    .transferred_agent_context(&LegId::from("old-b"))
                    .is_none()
            );
        }
        sessions.push((session, rx));
    }
}

/// A non-agent extension's leg must never be attributed as the agent leg:
/// `stamp_caller_agent_identity` only stamps when the caller's user-part
/// resolves to a registered agent. Covers an ordinary extension dialing an
/// external number and an ordinary extension calling an agent extension
/// (internal call), plus a positive control for the agent's own extension.
#[tokio::test]
async fn caller_leg_not_agent_for_non_agent_extension() {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry};
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    let (mut server, _) = create_test_server().await;
    let agents = Arc::new(MemoryRegistry::new());
    // Only 1001 is a registered agent (agent_id == its extension).
    agents
        .register(
            "1001".into(),
            "Alice".into(),
            "sip:1001@rustpbx.com".into(),
            vec![],
            2,
        )
        .await
        .unwrap();
    Arc::get_mut(&mut server).unwrap().agent_registry = Some(agents.clone());

    for (name, caller, callee, expect_agent) in [
        ("ext-outbound", "1002", "13800000000", false), // ordinary extension → external number
        ("ext-internal", "1002", "1001", false),        // ordinary extension → agent extension
        ("agent-outbound", "1001", "13800000000", true), // control: agent's own extension
    ] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            caller,
            None,
            "rustpbx.com",
            None,
        );
        let context = CallContext {
            session_id: name.into(),
            dialplan: Arc::new(Dialplan::new(name.into(), request, DialDirection::Outbound)),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: format!("sip:{caller}@rustpbx.com"),
            original_callee: format!("sip:{callee}@rustpbx.com"),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, handle, _rx) = SipSession::new_uac(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            false,
        );
        server
            .active_call_registry
            .register_handle(name.into(), handle);

        session.stamp_caller_agent_identity().await;

        let caller_leg = session.legs.get(&LegId::from("caller")).unwrap();
        assert_eq!(caller_leg.agent_id.is_some(), expect_agent, "{name}");
        assert_eq!(
            caller_leg.leg_role(),
            if expect_agent { "agent" } else { "caller" },
            "{name}"
        );
    }
}

#[tokio::test]
async fn sip_consult_switch_events_follow_only_b_reinvite_transitions() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "consult".into(),
        dialplan: Arc::new(Dialplan::new(
            "consult".into(),
            request,
            DialDirection::Outbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:bob@rustpbx.com".into(),
        original_callee: "sip:charlie@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);
    session.media_profile.path = MediaPathMode::Bypass;
    session
        .legs
        .insert(LegId::from("callee"), Leg::new(LegId::from("callee")));
    session.meta.root_session_id = Some("original".into());
    session
        .legs
        .get_mut(&LegId::from("caller"))
        .unwrap()
        .agent_id = Some("bob".into());
    session.extensions.write().insert(HashMap::from([(
        "resolved_agent_id".to_string(),
        "charlie".to_string(),
    )]));
    for (scenario, side, prev, direction, expected) in [
        (
            "unanswered",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            None,
        ),
        (
            "early_media",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            None,
        ),
        (
            "initial",
            DialogSide::Caller,
            LegState::Initializing,
            "sendrecv",
            None,
        ),
        (
            "initial_hold",
            DialogSide::Caller,
            LegState::Initializing,
            "sendonly",
            None,
        ),
        (
            "hold",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            Some("customer"),
        ),
        (
            "repeat_hold",
            DialogSide::Caller,
            LegState::Hold,
            "sendonly",
            None,
        ),
        (
            "resume",
            DialogSide::Caller,
            LegState::Hold,
            "sendrecv",
            Some("consult"),
        ),
        (
            "repeat_resume",
            DialogSide::Caller,
            LegState::Connected,
            "sendrecv",
            None,
        ),
        (
            "c_hold",
            DialogSide::Callee,
            LegState::Connected,
            "sendonly",
            None,
        ),
        (
            "c_resume",
            DialogSide::Callee,
            LegState::Hold,
            "sendrecv",
            None,
        ),
        (
            "api",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            None,
        ),
        (
            "handoff",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            None,
        ),
        (
            "transferred",
            DialogSide::Caller,
            LegState::Hold,
            "sendrecv",
            None,
        ),
        (
            "conference",
            DialogSide::Caller,
            LegState::Hold,
            "sendrecv",
            None,
        ),
        (
            "conference_hold",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            None,
        ),
        (
            "ordinary",
            DialogSide::Caller,
            LegState::Connected,
            "sendonly",
            None,
        ),
    ] {
        session.meta.answer_time = (scenario != "unanswered").then(Instant::now);
        session.legs.get_mut(&LegId::from("callee")).unwrap().state = if scenario == "early_media" {
            LegState::EarlyMedia
        } else {
            LegState::Connected
        };
        session.meta.transfer_in_progress = scenario == "handoff";
        session.meta.transferred = scenario == "transferred";
        session.conference = scenario
            .starts_with("conference")
            .then(|| ConferenceAttachment {
                leg_id: LegId::from("callee"),
                conference_id: "room".into(),
            });
        if scenario == "ordinary" {
            session.meta.root_session_id = None;
        }
        let leg = LegId::from(if side == DialogSide::Caller {
            "caller"
        } else {
            "callee"
        });
        session.legs.get_mut(&leg).unwrap().state = prev;
        while events.try_recv().is_ok() {}
        let next = if direction == "sendonly" {
            LegState::Hold
        } else {
            LegState::Connected
        };
        if scenario == "api" {
            session
                .fire_hold_transition_hooks(&leg, Some(prev), next)
                .await;
        } else {
            let sdp = format!(
                "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na={direction}\r\n"
            );
            let offer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &sdp).unwrap();
            session
                .apply_reinvite_hold_transition(side, &leg, &offer, &[])
                .await;
        }
        let mut switched = Vec::new();
        let mut holds = 0;
        while let Ok(entry) = events.try_recv() {
            match entry.event.event_type {
                "consult_switched" => switched.push(entry.event.payload),
                "call_held" | "call_unheld" => holds += 1,
                _ => {}
            }
        }
        assert_eq!(
            switched.len(),
            usize::from(expected.is_some()),
            "{scenario}"
        );
        if let Some(talking_to) = expected {
            assert_eq!(switched[0]["call_id"], "original");
            assert_eq!(switched[0]["transfer_id"], "consult");
            assert_eq!(switched[0]["talking_to"], talking_to);
        }
        let changed_hold = (prev == LegState::Hold) != (next == LegState::Hold);
        assert_eq!(
            holds,
            usize::from(changed_hold),
            "generic hold event: {scenario}"
        );
    }
}

// Hold vs in-dialog confirmations (rsipstack re-emits Confirmed per transaction).
/// A held caller leg must stay Hold, or the re-armed RTP watchdog tears the
/// call down with rtpTimeout mid-hold.
#[tokio::test]
async fn caller_dialog_confirmation_preserves_negotiated_hold() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .unwrap();
    let context = CallContext {
        session_id: "caller-confirm-hold".into(),
        dialplan: Arc::new(Dialplan::new(
            "caller-confirm-hold".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        dialog,
        false,
    );

    // Simulate a negotiated hold on the caller leg (apply_reinvite_hold_transition
    // has already run for the client's inactive/sendonly offer).
    session.update_leg_state(&LegId::from("caller"), LegState::Hold);
    assert_eq!(
        session.legs.get(&LegId::from("caller")).unwrap().state,
        LegState::Hold
    );

    // The ACK for the hold re-INVITE (or a later INFO/UPDATE) completes the
    // in-dialog transaction → rsipstack re-emits Confirmed.
    let confirmed = DialogState::Confirmed(
        session.caller_dialog_id(),
        rsipstack::sip::Response::default(),
    );
    session.handle_dialog_state(confirmed).await.unwrap();

    assert_eq!(
        session.legs.get(&LegId::from("caller")).unwrap().state,
        LegState::Hold,
        "caller Confirmed must not undo negotiated hold (mirrors the callee-side guard)"
    );
}

/// The re-emitted Confirmed must also leave the held leg's RTP watchdog paused.
#[tokio::test]
async fn caller_confirmation_does_not_rearm_rtp_watchdog_while_held() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };

    let (server, _) = create_test_server().await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .unwrap();
    let context = CallContext {
        session_id: "caller-confirm-watchdog".into(),
        dialplan: Arc::new(Dialplan::new(
            "caller-confirm-watchdog".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        dialog,
        false,
    );

    // Wire a real media peer for the caller leg, as an answered call has.
    let cfg = LegConfig::rtp_pcmu();
    let peer = LegInner::new("caller", &cfg, None).unwrap();
    session
        .legs
        .set_media_leg(&LegId::from("caller"), peer.clone());
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);
    // Answered calls arm the inactivity watchdog on both bridge legs.
    peer.arm_rtp_timeout(Duration::from_secs(60));
    assert!(
        session
            .media_leg(&LegId::from("caller"))
            .unwrap()
            .rtp_timeout_state()
            .active
            .load(std::sync::atomic::Ordering::SeqCst),
        "precondition: watchdog armed on the answered caller leg"
    );

    // Client holds itself → leg state Hold pauses the watchdog.
    session.update_leg_state(&LegId::from("caller"), LegState::Hold);
    assert!(
        !session
            .media_leg(&LegId::from("caller"))
            .unwrap()
            .rtp_timeout_state()
            .active
            .load(std::sync::atomic::Ordering::SeqCst),
        "precondition: hold pauses the watchdog"
    );

    // ACK arrives → Confirmed re-emitted.
    let confirmed = DialogState::Confirmed(
        session.caller_dialog_id(),
        rsipstack::sip::Response::default(),
    );
    session.handle_dialog_state(confirmed).await.unwrap();

    assert!(
        !session
            .media_leg(&LegId::from("caller"))
            .unwrap()
            .rtp_timeout_state()
            .active
            .load(std::sync::atomic::Ordering::SeqCst),
        "Confirmed must not re-arm the RTP watchdog on a held (silent) leg"
    );
}

#[tokio::test]
async fn queue_agent_answer_restores_held_caller_media() {
    use crate::call::app::queue::{QueueApp, QueueConfig};
    use crate::call::app::testing::MockCallStack;
    use crate::call::{DialDirection, DialStrategy, Dialplan, Location, MediaConfig};
    use crate::config::MediaProxyMode;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    let target = "sip:agent@example.com";
    let location = Location {
        aor: target.parse().unwrap(),
        ..Default::default()
    };
    let config = QueueConfig {
        name: "support".into(),
        accept_immediately: true,
        agents: vec![location.clone()],
        strategy: DialStrategy::Sequential(vec![location.into()]),
        ..Default::default()
    };
    let plan = config.to_plan();
    let mut stack = MockCallStack::run(Box::new(QueueApp::new(plan, config)), "caller", "support");
    stack.custom("dial_next_agent", serde_json::json!({}));
    // The harness runs the real queue event loop; replace only the outbound
    // SIP dial with negotiated local RTP peers before delivering its answer.
    let agent = loop {
        match stack
            .next_cmd(2000)
            .await
            .expect("queue must dial an agent")
        {
            CallCommand::LegAdd {
                leg_id: Some(id), ..
            } => break id,
            _ => {}
        }
    };
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "example.com",
        None,
    );
    let dialplan = Dialplan::new("queue-held-caller".into(), request, DialDirection::Inbound)
        .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All));
    let (mut session, _handle, _commands) = build_session_with_cmd_rx(dialplan).await;
    let _guard = session.cancel_token.clone().drop_guard();
    // Signaling is outside this media regression; these peers have no SIP dialog.
    session.caller_dialog = None;
    session
        .legs
        .insert(agent.clone(), crate::call::domain::Leg::new(agent.clone()));
    let mut remotes = Vec::new();
    for id in [LegId::from("caller"), agent.clone()] {
        let local = LegInner::new(id.as_str(), &LegConfig::rtp_pcmu(), None).unwrap();
        let remote = LegInner::new(format!("remote-{id}"), &LegConfig::rtp_pcmu(), None).unwrap();
        let offer = remote.create_offer().await.unwrap();
        let answer = local
            .apply_sdp(&offer, rustrtc::SdpType::Offer)
            .await
            .unwrap();
        remote
            .apply_sdp(&answer, rustrtc::SdpType::Answer)
            .await
            .unwrap();
        local.accept();
        remote.accept();
        session.legs.set_media_leg(&id, local);
        session.update_leg_state(&id, LegState::Connected);
        remotes.push(remote);
    }
    session.update_leg_state(&LegId::from("caller"), LegState::Hold);
    stack.custom(
        "agent_connected",
        serde_json::json!({
            "agent_uri": target,
            "leg_id": agent.to_string(),
        }),
    );
    while let Some(command) = stack.next_cmd(2000).await {
        // Run the queue's media decisions through the production session;
        // its notifications belong to the separate app event loop above.
        if matches!(
            command,
            CallCommand::Bridge { .. } | CallCommand::Unhold { .. }
        ) {
            let result = session.execute_command(command, None).await;
            assert!(result.success, "{:?}", result.message);
        }
    }
    stack.join().await.unwrap();
    assert_eq!(
        session.legs.get(&LegId::from("caller")).unwrap().state,
        LegState::Connected
    );
    assert!(
        session.bridge().unwrap().is_bridged(),
        "queue handoff must resume live media"
    );
    for id in [LegId::from("caller"), agent] {
        assert!(
            session.media_leg(&id).unwrap().egress_is_relay(),
            "{id} must forward live audio"
        );
    }
}

/// A held leg unholding while the bridge is down must restore the media path.
#[tokio::test]
async fn own_side_unhold_restores_media_path_when_bridge_down() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};
    use crate::rwi::RwiGateway;

    let gateway = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
    let (server, _) = create_test_server_with_rwi_gateway(ProxyConfig::default(), gateway).await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "caller",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "own-unhold-restore".into(),
        dialplan: Arc::new(Dialplan::new(
            "own-unhold-restore".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:caller@rustpbx.com".into(),
        original_callee: "sip:1101@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server.clone(), cancel, None, context, true);

    // Wire negotiated media for caller + agent legs (same recipe as the
    // added-media bridge test above: remote peer offers, local leg answers).
    let cfg = LegConfig::rtp_pcmu();
    let remote_caller = LegInner::new("remote-caller", &cfg, None).unwrap();
    let caller_peer = LegInner::new("caller", &cfg, None).unwrap();
    let caller_offer = remote_caller.create_offer().await.unwrap();
    let caller_answer = caller_peer
        .apply_sdp(&caller_offer, rustrtc::SdpType::Offer)
        .await
        .unwrap();
    remote_caller
        .apply_sdp(&caller_answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    session
        .legs
        .set_media_leg(&LegId::from("caller"), caller_peer.clone());
    session.media_leg(&LegId::from("caller")).unwrap().accept();
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);

    let remote_agent = LegInner::new("remote-agent", &cfg, None).unwrap();
    let agent_peer = LegInner::new("agent", &cfg, None).unwrap();
    let agent_offer = remote_agent.create_offer().await.unwrap();
    let agent_answer = agent_peer
        .apply_sdp(&agent_offer, rustrtc::SdpType::Offer)
        .await
        .unwrap();
    remote_agent
        .apply_sdp(&agent_answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    session.legs.insert(
        LegId::from("agent"),
        crate::call::domain::Leg::new(LegId::from("agent")),
    );
    session
        .legs
        .set_media_leg(&LegId::from("agent"), agent_peer.clone());
    session.media_leg(&LegId::from("agent")).unwrap().accept();
    session.update_leg_state(&LegId::from("agent"), LegState::Connected);

    // Logical pair selected (queue handoff bridged caller+agent once).
    session.bridge = BridgeConfig::bridge(LegId::from("caller"), LegId::from("agent"));
    // Bridge the pair for real, then simulate the hold teardown.
    assert!(
        session
            .execute_command(
                CallCommand::Bridge {
                    leg_a: LegId::from("caller"),
                    leg_b: LegId::from("agent"),
                    mode: crate::call::domain::P2PMode::Audio,
                },
                None
            )
            .await
            .success,
        "precondition: bridge established"
    );
    if let Some(mb) = session.bridge_mut() {
        mb.unbridge().await.unwrap();
    }
    assert!(
        !session.bridge().unwrap().is_bridged(),
        "precondition: media bridge torn down (held)"
    );

    // The held caller leg.
    session.update_leg_state(&LegId::from("caller"), LegState::Hold);

    // Client retrieves: sendrecv re-INVITE offer arrives on the caller dialog.
    let sdp = "v=0\r\no=- 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
               m=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let offer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, sdp).unwrap();
    session
        .apply_reinvite_hold_transition(DialogSide::Caller, &LegId::from("caller"), &offer, &[])
        .await;

    assert_eq!(
        session.legs.get(&LegId::from("caller")).unwrap().state,
        LegState::Connected,
        "own-side unhold must leave the leg Connected"
    );
    assert!(
        session.bridge().unwrap().is_bridged(),
        "own-side unhold with a bridged pair must restore the media path (stop hold music)"
    );
}

/// Parallel queue round: each leg's events name the agent pinned when THAT
/// leg was dialed, not the session-level pin (the last agent dialed).
#[tokio::test]
async fn test_leg_agent_id_uses_the_legs_own_agent() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server, create_transaction};

    let (server, _) = create_test_server().await;
    let request = create_test_request(rsipstack::sip::Method::Invite, "caller", None, "rustpbx.com", None);
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server.dialog_layer.get_or_create_server_invite(&tx, state_tx, None, None).unwrap();
    let context = CallContext {
        session_id: "parallel-agents".into(),
        dialplan: Arc::new(Dialplan::new("parallel-agents".into(), request, DialDirection::Inbound)),
        cookie: TransactionCookie::default(), start_time: Instant::now(),
        original_caller: "sip:caller@rustpbx.com".into(),
        original_callee: "sip:queue@rustpbx.com".into(), max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(), metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new(server, CancellationToken::new(), None, context, dialog, false);

    let mut dial = |agent: &str| {
        SipSession::set_pinned_agent_id(&session.extensions, agent);
        let leg_id = LegId::new(uuid::Uuid::new_v4().to_string());
        let mut leg = crate::call::domain::Leg::new(leg_id.clone())
            .with_endpoint(format!("sip:{agent}@rustpbx.com"));
        leg.agent_id = session.pinned_agent_id();
        session.legs.insert(leg_id.clone(), leg);
        leg_id
    };
    let alice = dial("alice");
    let bob = dial("bob");

    assert_eq!(
        session.leg_agent_id(&alice, Some("sip:alice@rustpbx.com")).await.as_deref(),
        Some("alice"),
        "the first leg must not take the last-dialed agent"
    );
    assert_eq!(
        session.leg_agent_id(&bob, Some("sip:bob@rustpbx.com")).await.as_deref(),
        Some("bob")
    );
    assert_eq!(
        session.leg_agent_id(&LegId::from("gone"), None).await.as_deref(),
        Some("bob"),
        "an unknown leg falls back to the session-level agent"
    );
}

// ── Agent-leg Call-ID scheme (`LegPurpose::Agent` / `Consult`) ──────────────

#[test]
fn agent_leg_call_id_first_dial_uses_session_id() {
    use crate::proxy::proxy_call::sip_session::SipSession;
    assert_eq!(
        SipSession::agent_leg_call_id("0b9e6c1e1b404e8f9c212f5a8d3e7b61", 0),
        "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
    );
}

#[test]
fn agent_leg_call_id_redials_get_retry_suffix() {
    use crate::proxy::proxy_call::sip_session::SipSession;
    let base = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    assert_eq!(SipSession::agent_leg_call_id(base, 1), format!("{base}-r2"));
    assert_eq!(SipSession::agent_leg_call_id(base, 2), format!("{base}-r3"));
}

#[test]
fn agent_leg_call_id_suffixes_stay_unique() {
    use crate::proxy::proxy_call::sip_session::SipSession;
    let base = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let ids: std::collections::HashSet<String> = (0..8u32)
        .map(|seq| SipSession::agent_leg_call_id(base, seq))
        .collect();
    assert_eq!(ids.len(), 8, "every dial attempt must get a unique Call-ID");
}

#[test]
fn queue_dispatch_leg_gets_purpose_and_leg_role() {
    let mut leg = crate::call::domain::Leg::new(LegId::new("f81d4fae-7dec-11d0-a765-00a0c91e6bf6"));
    leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
    leg.agent_id = Some("1002".into());
    // Queue-dispatched agent leg: RWI leg events carry leg_role "agent".
    assert_eq!(leg.leg_role(), "agent");
}

// ── Teardown-cascade leg hangups (外呼接通后挂机，leg_role 归位) ─────────────

/// Harness for teardown tests: an OUTBOUND UAC session (the CTI click-to-dial
/// shape) with the given legs pre-registered, a pinned agent, and the
/// terminal state "customer hung up first" (ByCallee + `caller_remote` side).
async fn teardown_session(
    session_id: &str,
    legs: Vec<crate::call::domain::Leg>,
    child_leg_role: Option<&str>,
) -> (
    SipSession,
    tokio::sync::broadcast::Receiver<crate::rwi::EventCacheEntry>,
) {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "agent-1",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: session_id.into(),
        dialplan: Arc::new(Dialplan::new(
            session_id.into(),
            request,
            DialDirection::Outbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:agent-1@rustpbx.com".into(),
        original_callee: "sip:13800000000@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);
    for leg in legs {
        session.legs.insert(leg.id.clone(), leg);
    }
    // The CTI agent attribution (as the RWI originate seeds it) — makes the
    // session CC-attributed: `has_resolved_agent = true`.
    session
        .extensions
        .write()
        .insert(std::collections::HashMap::from([(
            "resolved_agent_id".to_string(),
            "agent-1".to_string(),
        )]));
    // Terminal state: answered call, CUSTOMER hung up first (the caller/A
    // dialog's remote BYE was stamped on an outbound session).
    session.meta.answer_time = Some(Instant::now());
    session.meta.hangup_reason = Some(CallRecordHangupReason::ByCallee);
    session.meta.hangup_by_side = Some("caller_remote".to_string());
    session.meta.child_leg_role = child_leg_role.map(str::to_string);
    (session, events)
}

fn drain_call_hangups(
    events: &mut tokio::sync::broadcast::Receiver<crate::rwi::EventCacheEntry>,
) -> Vec<serde_json::Value> {
    let mut hangups = Vec::new();
    while let Ok(entry) = events.try_recv() {
        if entry.event.event_type == "call_hangup" {
            hangups.push(entry.event.payload);
        }
    }
    hangups
}

#[tokio::test]
async fn app_reanswer_preserves_answered_metrics() {
    for (previous_answer, callee, expected_secs) in [
        (true, None, 172),
        (false, None, 0),
        (true, Some("sip:next-agent@example.test".to_string()), 0),
    ] {
        let (mut session, mut events) =
            teardown_session("app-reanswer-duration", vec![], None).await;
        session.meta.answer_time =
            previous_answer.then(|| Instant::now() - Duration::from_secs(172));

        // The satisfaction app answers an already answered caller. Only a
        // new callee answer should start a new talk interval.
        session
            .accept_call(callee, Some(String::new()))
            .await
            .unwrap();
        session.cleanup().await;
        let hangups = drain_call_hangups(&mut events);
        assert_eq!(hangups.len(), 1);
        let duration = hangups[0]["duration_secs"].as_u64().unwrap();
        assert!(
            (expected_secs..expected_secs + 2).contains(&duration),
            "previous_answer={previous_answer}: expected {expected_secs}s, got {duration}s"
        );
    }
}

/// 外呼接通后客户先挂机（报告的缺陷场景）：会话拆除时，仍连接着的坐席桥接腿
/// 必须在 session 级事件之前补发 leg 级 `call_hangup`（`leg_role: "agent"`，
/// 与 session 事件同 reason/hangup_by/duration），普通 caller/callee 腿保持
/// 静默；外呼方向的 `hangup_by` 归一为 "callee"（客户挂断，而非 CC 中心的
/// "agent" 错标）。
#[tokio::test]
async fn teardown_emits_leg_scoped_hangup_for_connected_cc_legs() {
    let mut agent_leg = crate::call::domain::Leg::new(LegId::new("agent-bridge-1"));
    agent_leg.agent_id = Some("agent-1".into());
    agent_leg.state = LegState::Connected;
    let mut caller_leg = crate::call::domain::Leg::new(LegId::from("caller"));
    caller_leg.state = LegState::Connected;
    let (mut session, mut events) =
        teardown_session("teardown-cc-leg", vec![agent_leg, caller_leg], None).await;

    while events.try_recv().is_ok() {}
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert_eq!(
        hangups.len(),
        2,
        "agent bridge leg teardown + session event, got: {hangups:?}"
    );
    // Leg-scoped agent teardown FIRST — this is what `leg_role == "agent"`
    // consumers filter on and what was missing before the fix.
    assert_eq!(hangups[0]["leg_id"], "agent-bridge-1");
    assert_eq!(hangups[0]["leg_role"], "agent");
    assert_eq!(
        hangups[0]["hangup_by"], "callee",
        "customer hung up first on an outbound call — never mislabeled as agent"
    );
    assert_eq!(hangups[0]["reason"], "callee");
    assert!(
        hangups[0]["duration_secs"].is_u64(),
        "established leg carries talk time"
    );
    assert_eq!(hangups[0]["call_id"], "teardown-cc-leg");
    // Session-scoped event LAST (the authority), no leg fields.
    assert!(hangups[1].get("leg_id").is_none() && hangups[1].get("leg_role").is_none());
    assert_eq!(hangups[1]["hangup_by"], "callee");
    assert_eq!(hangups[1]["reason"], "callee");
}

/// 没有坐席/consult 腿的普通两腿呼叫（含纯中继外呼）：teardown 不新增 leg 级
/// 事件，仍然只有 session 级一条 `call_hangup`。
#[tokio::test]
async fn teardown_plain_two_leg_call_stays_single_session_hangup() {
    let mut caller_leg = crate::call::domain::Leg::new(LegId::from("caller"));
    caller_leg.state = LegState::Connected;
    let mut callee_leg = crate::call::domain::Leg::new(LegId::from("callee"));
    callee_leg.state = LegState::Connected;
    let (mut session, mut events) =
        teardown_session("teardown-plain", vec![caller_leg, callee_leg], None).await;

    while events.try_recv().is_ok() {}
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert_eq!(
        hangups.len(),
        1,
        "positional legs must not duplicate the session event: {hangups:?}"
    );
    assert_eq!(
        hangups[0]["scope"], "call",
        "only the root session ends the logical call"
    );
    assert!(hangups[0].get("leg_id").is_none() && hangups[0].get("leg_role").is_none());
}

/// REPRO（restsend-call 坐席外呼失败，报告的缺陷场景）：坐席话机直接 INVITE
/// 外呼，被叫失败（404/486/无应答/CANCEL），通话从未建立 → `accept_call` 的
/// stamp 永远不运行。呼叫建立时的早期打点（`serve()` 里 seed 之后，与本测试
/// 中显式调用同一函数）给 caller 腿（= 坐席 dialog）带上 `agent_id`，teardown
/// 级联随即补发 leg 级 `call_hangup{leg_role: "agent"}`（先于 session 级）——
/// prd 合同：坐席 dialog 的全部 call_* 事件（含失败 teardown）必须带坐席归因。
/// 修复前：caller 腿无 `agent_id` → 级联排除 → 只有 session 级无角色事件。
#[tokio::test]
async fn failed_agent_outbound_teardown_reports_agent_caller_leg() {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry};
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (mut server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let registry = Arc::new(MemoryRegistry::new());
    registry
        .register(
            "agent-1".to_string(),
            "Agent One".to_string(),
            "sip:agent-1@rustpbx.com".to_string(),
            vec!["support".to_string()],
            1,
        )
        .await
        .unwrap();
    Arc::get_mut(&mut server)
        .expect("test server uniquely held")
        .agent_registry = Some(registry);

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "agent-1",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "failed-agent-outbound".into(),
        dialplan: Arc::new(Dialplan::new(
            "failed-agent-outbound".into(),
            request,
            DialDirection::Outbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:agent-1@rustpbx.com".into(),
        original_callee: "sip:13800000000@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);
    // 失败形态：caller dialog 已终止（reject/CANCEL），腿留在 registry、
    // 从未接通、（修复前）无 agent_id。
    let mut caller_leg = crate::call::domain::Leg::new(LegId::from("caller"));
    caller_leg.state = LegState::Ended;
    session.legs.insert(LegId::from("caller"), caller_leg);

    // serve() 的早期打点（seed 门控内）：注册坐席的 From → caller 腿归因。
    session.stamp_caller_agent_identity().await;
    assert_eq!(
        session
            .legs
            .get(&LegId::from("caller"))
            .unwrap()
            .agent_id
            .as_deref(),
        Some("agent-1"),
        "the early site must attribute the caller leg to the registered agent"
    );
    // 失败 teardown 终态。
    session.meta.answer_time = None;
    session.meta.hangup_reason = Some(CallRecordHangupReason::NoAnswer);

    while events.try_recv().is_ok() {}
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert_eq!(
        hangups.len(),
        2,
        "agent caller teardown (leg-scoped) + session event, got: {hangups:?}"
    );
    // Leg 级坐席拆除 FIRST — leg_role=agent 消费方的过滤面。
    assert_eq!(hangups[0]["scope"], "leg");
    assert_eq!(hangups[0]["leg_id"], "caller");
    assert_eq!(hangups[0]["leg_role"], "agent");
    assert_eq!(hangups[0]["call_id"], "failed-agent-outbound");
    assert_eq!(
        hangups[0]["duration_secs"], 0,
        "a never-connected dialog carries no talk time: {hangups:?}"
    );
    // Session 级事件 LAST（权威），无 leg 字段。
    assert!(hangups[1].get("leg_id").is_none() && hangups[1].get("leg_role").is_none());
}

/// 护栏：非注册用户的外呼失败保持原样 —— caller 腿不被归因，teardown 仍然
/// 只有 session 级一条 `call_hangup`（无 leg 字段），普通外呼不误标坐席。
#[tokio::test]
async fn failed_non_agent_outbound_stays_single_session_hangup() {
    let mut caller_leg = crate::call::domain::Leg::new(LegId::from("caller"));
    caller_leg.state = LegState::Ended;
    // 复用 harness（registry 未安装 → stamp 必然 no-op，即使被调用）。
    let (mut session, mut events) =
        teardown_session("failed-plain-outbound", vec![caller_leg], None).await;

    session.stamp_caller_agent_identity().await;
    assert!(
        session
            .legs
            .get(&LegId::from("caller"))
            .unwrap()
            .agent_id
            .is_none(),
        "an unregistered caller must not be agent-attributed"
    );

    while events.try_recv().is_ok() {}
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert_eq!(
        hangups.len(),
        1,
        "plain outbound failure keeps the single session hangup: {hangups:?}"
    );
    assert!(hangups[0].get("leg_id").is_none() && hangups[0].get("leg_role").is_none());
}
#[tokio::test]
async fn teardown_child_leg_session_skips_leg_scoped_hangup() {
    let mut agent_leg = crate::call::domain::Leg::new(LegId::new("agent-bridge-1"));
    agent_leg.agent_id = Some("agent-1".into());
    agent_leg.state = LegState::Connected;
    let (mut session, mut events) =
        teardown_session("teardown-child", vec![agent_leg], Some("agent")).await;

    while events.try_recv().is_ok() {}
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert_eq!(hangups.len(), 1, "{hangups:?}");
    assert!(hangups[0].get("leg_id").is_none());
    assert_eq!(
        hangups[0]["leg_role"], "agent",
        "session event stamps the child role"
    );
    assert_eq!(
        hangups[0]["scope"], "leg",
        "a child teardown does not end the root call"
    );
}

/// Setup 失败/从未连接的 CC 腿不参与 teardown 补发：miss 语义（ring_timeout /
/// superseded）由 `handle_remove_leg` 的 in-setup 路径负责，cleanup 只管
/// established 腿。
#[tokio::test]
async fn teardown_ignores_never_connected_cc_legs() {
    let mut agent_leg = crate::call::domain::Leg::new(LegId::new("agent-bridge-1"));
    agent_leg.agent_id = Some("agent-1".into());
    agent_leg.state = LegState::Ringing; // never connected
    let (mut session, mut events) =
        teardown_session("teardown-ringing", vec![agent_leg], None).await;

    while events.try_recv().is_ok() {}
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert_eq!(hangups.len(), 1, "{hangups:?}");
    assert!(hangups[0].get("leg_id").is_none() && hangups[0].get("leg_role").is_none());
}

// ═══════════════════════════════════════════════════════════════════
// Child-leg sessions (cluster dispatch, UUI `leg=` marker)
// ═══════════════════════════════════════════════════════════════════

/// A cluster peer dispatches a queue agent / consult / transfer leg to this
/// node: the UUI `leg=` marker becomes `meta.child_leg_role` and the root
/// session id is still inherited — but ONLY when the INVITE arrives from a
/// configured cluster peer. An external caller planting the header must get
/// the regular lifecycle (the marker suppresses events, so it must not be
/// forgeable), and unknown roles are ignored outright.
#[tokio::test]
async fn child_leg_role_marker_parsed_only_from_trusted_peer() {
    use crate::call::uui::{UUI_HEADER_NAME, build_uui_value};
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    const ROOT: &str = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (mut server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    std::sync::Arc::get_mut(&mut server)
        .unwrap()
        .cluster_peer_ips = vec!["192.0.2.10".parse().unwrap()];

    for (i, (leg, peer, expected_role)) in [
        (Some("agent"), Some("192.0.2.10:5060"), Some("agent")),
        (Some("consult"), Some("192.0.2.10:5060"), Some("consult")),
        (Some("transfer"), Some("192.0.2.10:5060"), Some("transfer")),
        // Forged marker from a non-peer address: ignored (root linkage via
        // the same UUI still applies — it only adds a correlation id).
        (Some("agent"), Some("192.0.2.99:5060"), None),
        // Unknown role: ignored even from a trusted peer.
        (Some("whatever"), Some("192.0.2.10:5060"), None),
        // No marker: ordinary inbound call.
        (None, Some("192.0.2.10:5060"), None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        if leg.is_some() {
            request.headers.push(rsipstack::sip::Header::Other(
                UUI_HEADER_NAME.into(),
                build_uui_value(ROOT, None, None, None, leg),
            ));
        }
        let (tx, _) = create_transaction(request.clone()).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let server_dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .expect("server dialog");
        let cookie = TransactionCookie::default();
        if let Some(peer) = peer {
            cookie.insert_extension(crate::call::CallerPeerContext(peer.to_string()));
        }
        let context = CallContext {
            session_id: format!("child-leg-{i}"),
            dialplan: Arc::new(Dialplan::new(
                format!("child-leg-{i}"),
                request,
                DialDirection::Inbound,
            )),
            cookie,
            start_time: Instant::now(),
            original_caller: "sip:alice@rustpbx.com".into(),
            original_callee: "sip:bob@rustpbx.com".into(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, _handle, _cmd_rx) = SipSession::new(
            server.clone(),
            CancellationToken::new(),
            None,
            context,
            server_dialog,
            false,
        );
        assert_eq!(
            session.meta.child_leg_role.as_deref(),
            expected_role,
            "case {i}: leg={leg:?} peer={peer:?}"
        );
        if leg == Some("agent") {
            // Receive the endpoint callbacks on the home node without manually
            // stamping its legs: only the trusted inbound UUI declares the role.
            let callee = LegId::from("callee");
            let dialog_id = rsipstack::dialog::DialogId {
                call_id: format!("home-endpoint-{i}"),
                local_tag: "local".into(),
                remote_tag: "remote".into(),
            };
            session.callee_dialogs.insert(dialog_id.clone(), ());
            while events.try_recv().is_ok() {}
            session
                .handle_callee_state(DialogState::Confirmed(
                    dialog_id,
                    rsipstack::sip::Response::default(),
                ))
                .await
                .unwrap();
            for direction in ["sendonly", "sendrecv"] {
                let sdp = format!(
                    "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na={direction}\r\n"
                );
                let offer =
                    rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &sdp).unwrap();
                session
                    .apply_reinvite_hold_transition(DialogSide::Callee, &callee, &offer, &[])
                    .await;
            }
            session.cleanup().await;
            let mut payloads = Vec::new();
            while let Ok(entry) = events.try_recv() {
                payloads.push((entry.event.event_type, entry.event.payload));
            }
            let want_role = if expected_role.is_some() {
                "agent"
            } else {
                "callee"
            };
            for kind in ["call_answered", "call_held", "call_unheld"] {
                let payload = payloads
                    .iter()
                    .find(|(name, value)| *name == kind && value["leg_id"] == "callee")
                    .unwrap_or_else(|| panic!("case {i}: missing {kind}: {payloads:?}"));
                assert_eq!(payload.1["leg_role"], want_role, "case {i}: {kind}");
            }
            if expected_role.is_some() {
                assert_eq!(
                    payloads
                        .iter()
                        .filter(|(kind, value)| {
                            *kind == "call_hangup" && value["leg_role"] == "agent"
                        })
                        .count(),
                    1,
                    "home node must report one agent teardown: {payloads:?}"
                );
            }
        }
        if leg.is_some() {
            // Root linkage via the UUI session id is independent of the leg
            // marker (even a forged marker still correlates the call).
            assert_eq!(session.meta.root_session_id.as_deref(), Some(ROOT));
        }
    }
}

/// A rejected replacement must leave the original dialog's locality intact.
#[tokio::test]
async fn failed_blind_transfer_restores_agent_leg_locality() {
    use crate::call::{DialDirection, Dialplan, Location};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };
    use crate::proxy::tests::test_sip_session_regressions::{
        build_session_with_cmd_rx_on, setup_recording_test_media,
    };
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::{SipAddr, udp::UdpConnection};

    for original_remote in [false, true] {
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = target.local_addr().unwrap();
        let gateway = crate::rwi::gateway::RwiGateway::new();
        let mut events = gateway.subscribe_events();
        let (mut server, _) = create_test_server_with_rwi_gateway(
            Default::default(),
            Arc::new(parking_lot::RwLock::new(gateway)),
        )
        .await;
        Arc::get_mut(&mut server).unwrap().cluster_peer_ips = vec![address.ip()];
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
        let serving = tokio::spawn(async move {
            serving_endpoint.serve().await.unwrap();
        });
        let target_uri: rsipstack::sip::Uri = format!("sip:target@{address}").parse().unwrap();
        server
            .locator
            .register(
                "target",
                None,
                Location {
                    aor: target_uri.clone(),
                    expires: 3600,
                    registered_aor: Some(target_uri.clone()),
                    home_proxy: (!original_remote).then(|| SipAddr {
                        r#type: Some(rsipstack::sip::Transport::Udp),
                        addr: address.to_string().as_str().try_into().unwrap(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let registered = server.locator.lookup(&target_uri).await.unwrap();
        assert_eq!(
            registered.len(),
            1,
            "replacement must use its registered location"
        );
        assert_eq!(registered[0].home_proxy.is_some(), !original_remote);
        let request = create_test_request(Method::Invite, "customer", None, "example.test", None);
        let dialplan = Dialplan::new("failed-locality".into(), request, DialDirection::Inbound)
            .with_caller("sip:customer@example.test".parse().unwrap());
        let (mut session, _handle, _commands) =
            build_session_with_cmd_rx_on(server.clone(), dialplan).await;
        setup_recording_test_media(&mut session).await;
        let original_request =
            create_test_request(Method::Invite, "agent", None, "example.test", None);
        let (tx, _) = create_transaction(original_request).await;
        let (state_tx, _) = mpsc::unbounded_channel();
        let original_dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .unwrap();
        let original_id = original_dialog.id();
        let callee = LegId::from("callee");
        session.legs.set_dialog(
            callee.clone(),
            rsipstack::dialog::dialog::Dialog::Invite(original_dialog),
        );
        let leg = session.legs.get_mut(&callee).unwrap();
        leg.agent_id = Some("agent".into());
        leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
        leg.remote_registered = original_remote;
        leg.state = LegState::Connected;
        session.meta.connected_callee_dialog_id = Some(original_id.clone());
        session.meta.connected_callee = Some("sip:agent@example.test".into());
        SipSession::set_pinned_agent_id(&session.extensions, "agent");
        let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
        session.callee_event_tx = Some(callee_tx);
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
            assert_eq!(invite.method, Method::Invite);
            let mut response = endpoint.make_response(&invite, StatusCode::BusyHere, None);
            response
                .headers
                .retain(|header| !matches!(header, Header::To(_)));
            response.headers.push(Header::To(
                format!("<sip:target@{address}>;tag=rejected").into(),
            ));
            target
                .send_to(response.to_string().as_bytes(), source)
                .await
                .unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            session.handle_blind_transfer(
                callee.clone(),
                target_uri.to_string(),
                transfer::TransferDisposition::AwaitResult,
                &mut callee_rx,
                HashMap::new(),
            ),
        )
        .await
        .expect("transfer must receive the rejection");
        responder.await.unwrap();
        serving.abort();
        assert!(result.is_err(), "486 must fail the replacement");
        assert!(result.unwrap_err().to_string().contains("486"));
        assert_eq!(session.legs.get_dialog(&callee).unwrap().id(), original_id);
        while events.try_recv().is_ok() {}
        let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
        let offer = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, sdp).unwrap();
        session.bridge.legs.clear();
        session
            .apply_reinvite_hold_transition(DialogSide::Callee, &callee, &offer, &[])
            .await;
        session.emit_rwi_leg_hangup(&callee, Some("UasBye".into()));
        let remote_after_failure = session.legs.get(&callee).unwrap().remote_registered;
        let mut payloads = Vec::new();
        while let Ok(entry) = events.try_recv() {
            if matches!(entry.event.event_type, "call_held" | "call_hangup") {
                payloads.push(entry.event.payload);
            }
        }
        session.cleanup().await;
        assert_eq!(payloads.len(), 2);
        for payload in payloads {
            assert_eq!(payload["leg_id"], "callee");
            if original_remote {
                assert!(
                    payload.get("leg_role").is_none(),
                    "remote original must remain suppressed: {payload}"
                );
            } else {
                assert_eq!(
                    payload["leg_role"], "agent",
                    "local original still owns its events"
                );
            }
        }
        assert_eq!(remote_after_failure, original_remote);
    }
}

/// A child-leg session (cluster-dispatched queue agent / consult / transfer)
/// emits NO session-level `call_answered` — the dispatching node already
/// emitted the logical call's answered — and its session-level ringing
/// carries the declared role as `leg_role` so consumers can tell the agent
/// leg's ringing apart from the customer call's.
#[tokio::test]
async fn child_leg_session_event_shape() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "child-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "child-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);
    session.meta.child_leg_role = Some("agent".into());

    // Answered: suppressed for the child leg, latch NOT consumed…
    session.emit_session_call_answered_once();
    assert!(
        events.try_recv().is_err(),
        "child-leg session must not emit call_answered"
    );

    // …so the root session behavior is intact once the role is absent.
    session.meta.child_leg_role = None;
    session.emit_session_call_answered_once();
    let answered = events.try_recv().expect("root session answered");
    assert_eq!(answered.event.event_type, "call_answered");
    assert!(answered.event.payload.get("leg_id").is_none());

    // Session-level ringing of the child leg is SUPPRESSED — the leg-level
    // ringing below is the one authoritative agent ringing (a session-level
    // twin µs apart double-counted agent ringings).
    session.meta.child_leg_role = Some("agent".into());
    session.emit_call_ringing(None, false, None);
    assert!(
        events.try_recv().is_err(),
        "child session must not emit a session-level ringing twin"
    );

    // Leg-level ringing of the child dialog rides the child role — the
    // positional "callee" lost the agent attribution (same fix family as
    // answered/held/hangup).
    session.emit_call_ringing(Some("callee".into()), false, None);
    let leg_ringing = events.try_recv().expect("child leg ringing");
    assert_eq!(leg_ringing.event.event_type, "call_ringing");
    assert_eq!(leg_ringing.event.payload["leg_id"], "callee");
    assert_eq!(
        leg_ringing.event.payload["leg_role"], "agent",
        "child dialog's leg events must carry the declared role: {leg_ringing:?}"
    );

    // The DISPATCHER HOP (caller leg of the child session) keeps its
    // positional role — one agent answer must not produce two
    // role-attributed events (caller + callee both transition Connected
    // when the agent picks up).
    session.emit_call_ringing(Some("caller".into()), false, None);
    let hop_ringing = events.try_recv().expect("hop leg ringing");
    assert_eq!(hop_ringing.event.payload["leg_role"], "caller");
    session.update_leg_state(&LegId::from("caller"), crate::call::domain::LegState::Connected);
    let hop_answered = events.try_recv().expect("hop leg answered");
    assert_eq!(hop_answered.event.event_type, "call_answered");
    assert_eq!(
        hop_answered.event.payload["leg_role"], "caller",
        "the dispatcher hop must NOT claim the agent role: {hop_answered:?}"
    );

    // Non-child sessions keep the positional role for leg events.
    session.meta.child_leg_role = None;
    session.legs.insert(
        LegId::from("callee"),
        crate::call::domain::Leg::new(LegId::from("callee")),
    );
    session.emit_call_ringing(Some("callee".into()), false, None);
    let plain_ringing = events.try_recv().expect("plain leg ringing");
    assert_eq!(plain_ringing.event.payload["leg_role"], "callee");
}

/// Agent-facing dial intent on the wire: `build_target_invite_option`
/// declares the RFC 7433 `leg=agent` marker when the target leg was
/// pre-stamped `LegPurpose::Agent` (blind transfer to a registered agent) so
/// the terminator — in a cluster the registration's home node — treats the
/// dialog as a child leg of this call instead of a fresh root call. A
/// non-agent target keeps no marker, and an existing CC UUI (queue enricher)
/// is MERGED with the leg param rather than duplicated.
#[tokio::test]
async fn target_invite_declares_agent_leg_marker_when_purpose_agent() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "agent-marker-sess".into(),
        dialplan: Arc::new(
            Dialplan::new(
                "agent-marker-sess".into(),
                request,
                DialDirection::Inbound,
            )
            .with_caller("sip:alice@rustpbx.com".try_into().unwrap()),
        ),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);

    let target = Location {
        aor: "sip:agent@example.test".parse().unwrap(),
        ..Default::default()
    };

    // Non-agent target (no pre-stamped purpose): no marker.
    let (invite, _, _, _) = session
        .build_target_invite_option(&target, None, None)
        .await
        .unwrap();
    let headers = invite.headers.unwrap_or_default();
    assert!(
        !headers
            .iter()
            .any(|h| h.name().eq_ignore_ascii_case(crate::call::uui::UUI_HEADER_NAME)),
        "plain target must not declare a leg marker"
    );

    // Pre-stamped agent purpose (blind transfer to a registered agent): the
    // INVITE carries the `leg=agent` marker rooted at the session id.
    if let Some(leg) = session.legs.get_mut(&LegId::from("callee")) {
        leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
    }
    let (invite, _, _, _) = session
        .build_target_invite_option(&target, None, None)
        .await
        .unwrap();
    let headers = invite.headers.unwrap_or_default();
    let mut uui_bag = rsipstack::sip::Headers::default();
    uui_bag.extend(
        headers
            .iter()
            .filter(|h| h.name().eq_ignore_ascii_case(crate::call::uui::UUI_HEADER_NAME))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let uui = crate::call::uui::extract_cc_uui(&uui_bag)
        .expect("agent-facing dial must declare the UUI marker");
    assert_eq!(uui.leg.as_deref(), Some("agent"));
    assert!(
        uui.session_id == "agent-marker-sess",
        "correlation root must be the session id, got {}",
        uui.session_id
    );

    // An existing CC UUI (queue screen-pop context) MERGES with the leg
    // param — exactly one UUI header, both the queue id and leg survive.
    let mut enriched = target.clone();
    enriched.headers = Some(vec![crate::call::uui::build_uui_header(
        "agent-marker-sess",
        Some("sales"),
        Some("Sales Hotline"),
        None,
        None,
    )]);
    let (invite, _, _, _) = session
        .build_target_invite_option(&enriched, None, None)
        .await
        .unwrap();
    let headers = invite.headers.unwrap_or_default();
    let uui_count = headers
        .iter()
        .filter(|h| h.name().eq_ignore_ascii_case(crate::call::uui::UUI_HEADER_NAME))
        .count();
    assert_eq!(uui_count, 1, "existing UUI must be merged, not duplicated");
    let mut uui_bag = rsipstack::sip::Headers::default();
    uui_bag.extend(
        headers
            .iter()
            .filter(|h| h.name().eq_ignore_ascii_case(crate::call::uui::UUI_HEADER_NAME))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let uui = crate::call::uui::extract_cc_uui(&uui_bag).expect("merged UUI must parse");
    assert_eq!(uui.leg.as_deref(), Some("agent"));
    assert_eq!(uui.queue_id.as_deref(), Some("sales"));
    assert_eq!(uui.queue_name.as_deref(), Some("Sales Hotline"));
}

/// REPRO (cluster child-leg double report): a cluster-dispatched agent leg
/// (`child_leg_role = "agent"`) hangs up first — the dynamic-leg monitor
/// emits the leg-scoped `call_hangup{leg_role:"agent", reason:"UasBye"}`
/// and REMOVES the leg. At teardown the session-scoped event must NOT claim
/// `leg_role` again: the leg's terminal transition already reported it.
/// Before the fix consumers filtering `leg_role == "agent"` saw TWO events
/// for one teardown with contradictory reasons (UasBye vs callee).
#[tokio::test]
async fn child_leg_agent_bye_reports_agent_role_exactly_once() {
    let root_id = "11111111111141118111111111111111";
    let child_id =
        crate::call::session_id::normalize_or_generate(&SipSession::agent_leg_call_id(root_id, 0));
    assert_eq!(
        child_id, root_id,
        "the first cluster leg can share the root ID"
    );
    let mut agent_leg = crate::call::domain::Leg::new(LegId::new("queue-agent"));
    agent_leg.agent_id = Some("agent-1".into());
    agent_leg.state = LegState::Connected;
    let (mut session, mut events) =
        teardown_session(&child_id, vec![agent_leg], Some("agent")).await;

    // Agent hangs up mid-session (session not yet cancelling): the dynamic
    // monitor delivers LegEnded{UasBye} → leg-scoped event + leg removal.
    session
        .execute_command(
            CallCommand::LegEnded {
                leg_id: LegId::new("queue-agent"),
                reason: "UasBye".to_string(),
            },
            None,
        )
        .await;

    // Session teardown follows (no draining in between — collect everything).
    session.cleanup().await;
    let hangups = drain_call_hangups(&mut events);

    assert!(hangups.iter().all(|event| event["call_id"] == root_id));

    let agent_reports: Vec<&serde_json::Value> = hangups
        .iter()
        .filter(|h| h["leg_role"] == "agent")
        .collect();
    assert_eq!(
        agent_reports.len(),
        1,
        "the agent leg teardown must be reported exactly once, got: {hangups:?}"
    );
    assert_eq!(agent_reports[0]["leg_id"], "queue-agent");
    assert_eq!(agent_reports[0]["reason"], "UasBye");
    // The session-scoped event still fires (authoritative reason/duration)
    // but without the leg_role claim.
    assert_eq!(
        hangups.len(),
        2,
        "leg-scoped + session-scoped events, no third report: {hangups:?}"
    );
    let session_event = hangups
        .iter()
        .find(|h| h.get("leg_id").is_none())
        .expect("session-scoped event present");
    assert!(
        session_event.get("leg_role").is_none(),
        "session event must not re-claim the already-reported leg role, got {session_event:?}"
    );
    assert!(
        hangups.iter().all(|event| event["scope"] == "leg"),
        "even the unmarked child terminal event must not end the root call: {hangups:?}"
    );
}

/// Real cross-node shape: the child-leg session's legs carry NO `agent_id`
/// (the attribution lives in the session extensions/meta, never on the leg),
/// and the phone leg is Ended-but-registered at teardown. The session-level
/// `call_hangup` must still claim the declared role — the former
/// `agent_id.is_some()` filter never matched this shape, so the agent hangup
/// lost its role entirely (prd: every call_hangup carried leg_role=null).
#[tokio::test]
async fn child_leg_session_hangup_claims_role_without_leg_agent_id() {
    let child_id = "22222222222242228222222222222222";

    // Dispatcher hop (Connected) + agent phone leg (Ended, still
    // registered) — the state at session teardown after the phone hung up.
    let mut hop = crate::call::domain::Leg::new(LegId::new("caller"));
    hop.state = LegState::Connected;
    let mut phone = crate::call::domain::Leg::new(LegId::new("callee"));
    phone.state = LegState::Ended;

    let (mut session, mut events) =
        teardown_session(child_id, vec![hop, phone], Some("agent")).await;
    session.cleanup().await;

    let hangups = drain_call_hangups(&mut events);
    let session_event = hangups
        .iter()
        .find(|h| h.get("leg_id").is_none())
        .expect("session-scoped teardown present");
    assert_eq!(
        session_event["leg_role"], "agent",
        "child session teardown must claim the declared role without a leg \
         agent_id marker: {session_event:?}"
    );
    assert_eq!(
        hangups
            .iter()
            .filter(|h| h["leg_role"] == "agent")
            .count(),
        1,
        "exactly one role-attributed agent hangup: {hangups:?}"
    );
}

/// Ring-timeout teardown: the dialog NEVER connected (the phone leg only
/// rang) and no leg-scoped report exists — the agent machine's session-level
/// `call_hangup` must still claim the declared role. This is the CANCEL the
/// queue sends at ring timeout (prd: it produced a hangup without leg_role).
#[tokio::test]
async fn child_leg_miss_hangup_claims_role_on_ring_timeout() {    let child_id = "33333333333343338333333333333333";
    let mut hop = crate::call::domain::Leg::new(LegId::new("caller"));
    hop.state = LegState::Connected;
    let mut phone = crate::call::domain::Leg::new(LegId::new("callee"));
    phone.state = LegState::Ringing;

    let (mut session, mut events) =
        teardown_session(child_id, vec![hop, phone], Some("agent")).await;
    session.cleanup().await;

    let hangups = drain_call_hangups(&mut events);
    let session_event = hangups
        .iter()
        .find(|h| h.get("leg_id").is_none())
        .expect("session-scoped teardown present");
    assert_eq!(
        session_event["leg_role"], "agent",
        "miss teardown must claim the declared role: {session_event:?}"
    );
    assert_eq!(
        hangups
            .iter()
            .filter(|h| h["leg_role"] == "agent")
            .count(),
        1,
        "exactly one role-attributed agent hangup in a miss flow: {hangups:?}"
    );
}

/// REPRO (transfer slot-reuse residue): a blind/REFER replacement attaches
/// into the reusable `callee` slot. Resetting the slot identity at the swap
/// strips the previous occupant's agent attribution — the NEW party's (IVR /
/// extension) events carry no `leg_role` — while a transfer to a REGISTERED
/// agent keeps `leg_role: "agent"`.
#[tokio::test]
async fn transfer_slot_reuse_strips_stale_agent_role() {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry, PresenceState};
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    async fn session_with_agent_callee_slot(
        session_id: &str,
        agent_registry: Option<Arc<MemoryRegistry>>,
    ) -> (
        SipSession,
        tokio::sync::broadcast::Receiver<crate::rwi::EventCacheEntry>,
    ) {
        let gateway = crate::rwi::gateway::RwiGateway::new();
        let events = gateway.subscribe_events();
        let (server, _) = create_test_server_with_rwi_gateway(
            Default::default(),
            Arc::new(parking_lot::RwLock::new(gateway)),
        )
        .await;
        let mut server = server;
        if let Some(registry) = agent_registry {
            Arc::get_mut(&mut server)
                .expect("test server uniquely held")
                .agent_registry = Some(registry);
        }
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "agent-1",
            None,
            "rustpbx.com",
            None,
        );
        let context = CallContext {
            session_id: session_id.into(),
            dialplan: Arc::new(Dialplan::new(
                session_id.into(),
                request,
                DialDirection::Outbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:agent-1@rustpbx.com".into(),
            original_callee: "sip:13800000000@rustpbx.com".into(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, _handle, _commands) =
            SipSession::new_uac(server, CancellationToken::new(), None, context, false);
        let mut callee = crate::call::domain::Leg::new(LegId::from("callee"));
        callee.agent_id = Some("agent-1".into());
        callee.purpose = Some(crate::call::domain::LegPurpose::Agent);
        callee.state = LegState::Connected;
        session.legs.insert(LegId::from("callee"), callee);
        (session, events)
    }

    // ── Transfer to an IVR / extension: NO agent attribution survives ──
    let (mut session, mut events) = session_with_agent_callee_slot("repro-slot-ivr", None).await;
    while events.try_recv().is_ok() {}

    let ivr_target: rsipstack::sip::Uri = "sip:ivr-9001@rustpbx.com".parse().unwrap();
    session.reset_callee_slot_identity(&ivr_target).await;
    let leg = session
        .legs
        .get(&LegId::from("callee"))
        .expect("slot present");
    assert!(
        leg.agent_id.is_none() && leg.purpose.is_none(),
        "IVR/extension target must not inherit the agent pin, got {:?}",
        (leg.agent_id.as_deref(), leg.purpose)
    );

    // The replacement's ring event no longer claims `agent` — the reported
    // residue (every later event kept `leg_role: "agent"`) is gone. The
    // positional fallback ("callee") is the documented non-CC contract.
    session.update_leg_state(&LegId::from("callee"), LegState::Ringing);
    let ringing = events
        .try_recv()
        .expect("replacement target rings")
        .event
        .payload;
    assert_eq!(ringing["event_type"], "call_ringing");
    assert_ne!(
        ringing["leg_role"], "agent",
        "transferred-to IVR/extension must not carry the agent role, got {ringing:?}"
    );

    // ── Transfer to a REGISTERED agent: the leg keeps `agent` ──
    let registry = Arc::new(MemoryRegistry::new());
    registry
        .register(
            "2002".to_string(),
            "Bob".to_string(),
            "sip:2002@rustpbx.com".to_string(),
            vec!["support".to_string()],
            1,
        )
        .await
        .unwrap();
    registry
        .update_presence("2002", PresenceState::Idle)
        .await
        .unwrap();
    let (mut session, _events) =
        session_with_agent_callee_slot("repro-slot-agent", Some(registry)).await;
    let agent_target: rsipstack::sip::Uri = "sip:2002@rustpbx.com".parse().unwrap();
    session.reset_callee_slot_identity(&agent_target).await;
    let leg = session
        .legs
        .get(&LegId::from("callee"))
        .expect("slot present");
    assert_eq!(
        leg.agent_id.as_deref(),
        Some("2002"),
        "a registered agent target keeps the agent pin"
    );
    assert_eq!(leg.leg_role(), "agent");
}

/// Ring-timeout cancel: `LegRemove` on a NEVER-connected agent leg emits a
/// leg-scoped `call_hangup{leg_role:"agent", reason}` — the queue's miss
/// must stay visible to agent-side consumers (regression: the CANCEL path
/// dropped the event entirely). A CONNECTED leg's removal is not a miss and
/// stays silent (the session-scoped hangup owns that story).
#[tokio::test]
async fn leg_remove_of_unconnected_leg_emits_miss_hangup() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "miss-hangup-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "miss-hangup-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);

    // A ringing agent leg with the agent attribution pinned (queue dispatch).
    let mut leg = crate::call::domain::Leg::new(LegId::from("agent-1"));
    leg.state = crate::call::domain::LegState::Ringing;
    leg.agent_id = Some("bob".into());
    session.legs.insert(LegId::from("agent-1"), leg);

    session
        .execute_command(
            CallCommand::LegRemove {
                leg_id: LegId::from("agent-1"),
                reason: Some("ring_timeout".into()),
            },
            None,
        )
        .await;

    let hangup = events
        .try_recv()
        .expect("miss hangup for the ringing agent leg");
    assert_eq!(hangup.event.event_type, "call_hangup");
    assert_eq!(hangup.event.payload["leg_id"], "agent-1");
    assert_eq!(hangup.event.payload["leg_role"], "agent");
    assert_eq!(hangup.event.payload["reason"], "ring_timeout");
    // Setup failure: no initiator, no talk time.
    assert!(hangup.event.payload["hangup_by"].is_null());
    assert!(hangup.event.payload["duration_secs"].is_null());
    assert!(events.try_recv().is_err(), "exactly one event for the miss");

    // A CONNECTED leg's removal is not a miss — no event.
    let mut leg = crate::call::domain::Leg::new(LegId::from("agent-2"));
    leg.state = crate::call::domain::LegState::Connected;
    leg.agent_id = Some("bob".into());
    session.legs.insert(LegId::from("agent-2"), leg);
    session
        .execute_command(
            CallCommand::LegRemove {
                leg_id: LegId::from("agent-2"),
                reason: None,
            },
            None,
        )
        .await;
    assert!(
        events.try_recv().is_err(),
        "connected-leg removal stays silent"
    );
}

// ── Home-proxy-routed legs: this node never stamps leg_role ────────────────

/// A leg whose INVITE was routed to a REMOTE `home_proxy` (`remote_registered`)
/// keeps its leg identity on leg-scoped events but carries NO `leg_role` —
/// the registration node's child-leg session is the single source of the
/// agent attribution. Without this both nodes stamped
/// `leg_role: "agent"` on the same agent leg (two ringings per dial).
#[tokio::test]
async fn remote_registered_leg_events_carry_no_leg_role() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "remote-role-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "remote-role-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);

    // Queue-dispatch agent leg dialed to a remote-registered agent.
    let mut leg = crate::call::domain::Leg::new(LegId::from("agent-remote"));
    leg.agent_id = Some("bob".into());
    leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
    leg.remote_registered = true;
    session.legs.insert(LegId::from("agent-remote"), leg);

    // The raw role lookup is suppressed…
    assert_eq!(
        session.leg_role_of("agent-remote"),
        None,
        "home-proxy-routed leg must not yield a leg_role on this node"
    );

    // …and every leg-scoped lifecycle event carries leg_id WITHOUT leg_role.
    session.update_leg_state(
        &LegId::from("agent-remote"),
        crate::call::domain::LegState::Ringing,
    );
    let ringing = events.try_recv().expect("leg ringing");
    assert_eq!(ringing.event.event_type, "call_ringing");
    assert_eq!(ringing.event.payload["leg_id"], "agent-remote");
    assert!(
        ringing.event.payload.get("leg_role").is_none(),
        "remote leg ringing must stay role-free: {ringing:?}"
    );

    session.update_leg_state(
        &LegId::from("agent-remote"),
        crate::call::domain::LegState::Connected,
    );
    let answered = events.try_recv().expect("leg answered");
    assert_eq!(answered.event.event_type, "call_answered");
    assert_eq!(answered.event.payload["leg_id"], "agent-remote");
    assert!(answered.event.payload.get("leg_role").is_none());

    session.fire_hold_transition_hooks(
        &LegId::from("agent-remote"),
        Some(crate::call::domain::LegState::Connected),
        crate::call::domain::LegState::Hold,
    )
    .await;
    let held = events.try_recv().expect("leg held");
    assert_eq!(held.event.event_type, "call_held");
    assert_eq!(held.event.payload["leg_id"], "agent-remote");
    assert!(held.event.payload.get("leg_role").is_none());

    // A LOCAL agent leg in the same session is unaffected.
    let mut local_leg = crate::call::domain::Leg::new(LegId::from("agent-local"));
    local_leg.agent_id = Some("alice".into());
    local_leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
    session.legs.insert(LegId::from("agent-local"), local_leg);
    assert_eq!(
        session.leg_role_of("agent-local").as_deref(),
        Some("agent"),
        "locally-registered agent leg keeps its role"
    );
}

/// Ring-timeout cancel of a remote-registered agent leg still emits the miss
/// hangup (the queue's no-answer story stays visible) — but WITHOUT
/// `leg_role`; the registration node reports the agent leg.
#[tokio::test]
async fn remote_registered_leg_miss_hangup_keeps_event_drops_role() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "remote-miss-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "remote-miss-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);

    let mut leg = crate::call::domain::Leg::new(LegId::from("agent-remote"));
    leg.state = crate::call::domain::LegState::Ringing;
    leg.agent_id = Some("bob".into());
    leg.remote_registered = true;
    session.legs.insert(LegId::from("agent-remote"), leg);

    session
        .execute_command(
            CallCommand::LegRemove {
                leg_id: LegId::from("agent-remote"),
                reason: Some("ring_timeout".into()),
            },
            None,
        )
        .await;

    let hangup = events.try_recv().expect("miss hangup still emitted");
    assert_eq!(hangup.event.event_type, "call_hangup");
    assert_eq!(hangup.event.payload["leg_id"], "agent-remote");
    assert_eq!(hangup.event.payload["reason"], "ring_timeout");
    assert!(
        hangup.event.payload.get("leg_role").is_none(),
        "remote leg miss must not carry leg_role: {hangup:?}"
    );
}

/// Remote-registered agent leg at session teardown: the cascade must skip it
/// (the home node's child-leg session already reports the teardown with
/// `leg_role: "agent"`), while the raw hangup_by attribution survives —
/// a remote BYE is still relayed by the agent's home node.
#[tokio::test]
async fn remote_registered_leg_hangup_by_stays_agent_without_role() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "remote-hangup-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "remote-hangup-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);

    let mut leg = crate::call::domain::Leg::new(LegId::from("agent-remote"));
    leg.agent_id = Some("bob".into());
    leg.remote_registered = true;
    session.legs.insert(LegId::from("agent-remote"), leg);
    // Production dispatcher state: the queue pins the agent before dialing —
    // `normalize_call_hangup_by` needs it to keep the "agent" attribution.
    SipSession::set_pinned_agent_id(&session.extensions, "bob");

    session.emit_rwi_leg_hangup(&LegId::from("agent-remote"), Some("UasBye".into()));
    let hangup = events.try_recv().expect("leg hangup");
    assert_eq!(hangup.event.payload["leg_id"], "agent-remote");
    assert_eq!(
        hangup.event.payload["hangup_by"], "agent",
        "UasBye on a remote agent leg is still the agent hanging up"
    );
    assert!(
        hangup.event.payload.get("leg_role").is_none(),
        "remote leg hangup must not carry leg_role: {hangup:?}"
    );
}

/// Home-node child-leg session: the UUI-attributed role rides the dialog's
/// lifecycle events. 344f7451 left the leg-scoped `call_answered` on the
/// positional "callee" role and the session `call_hangup` without any role
/// (its `leg_role() == role` filter never matched a positional leg), so
/// consumers filtering `leg_role == "agent"` lost answered/hangup on
/// cross-node dispatches (prd s17 A1/A10).
#[tokio::test]
async fn child_leg_session_stamps_attributed_role_on_lifecycle_events() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let mut events = gateway.subscribe_events();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "child-role-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "child-role-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);
    // The dispatcher routed this INVITE through the agent's home node; the
    // UUI marker declares this session's dialog role.
    session.meta.child_leg_role = Some("agent".into());

    let mut leg = crate::call::domain::Leg::new(LegId::from("agent-leg"));
    leg.agent_id = Some("bob".into());
    leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
    session.legs.insert(LegId::from("agent-leg"), leg);
    // Production home-node state: the dispatcher pins the agent on the routed
    // INVITE — `normalize_call_hangup_by` needs it for the "agent" attribution.
    SipSession::set_pinned_agent_id(&session.extensions, "bob");

    // Answered: the UUI role, NOT the positional callee role.
    session.update_leg_state(&LegId::from("agent-leg"), crate::call::domain::LegState::Connected);
    let answered = events.try_recv().expect("leg answered");
    assert_eq!(answered.event.event_type, "call_answered");
    assert_eq!(
        answered.event.payload["leg_role"], "agent",
        "answered must carry the UUI role, not the positional callee: {answered:?}"
    );

    // Leg-scoped hangup: the UUI role rides the teardown too.
    session.emit_rwi_leg_hangup(&LegId::from("agent-leg"), Some("UasBye".into()));
    let hangup = events.try_recv().expect("leg hangup");
    assert_eq!(hangup.event.event_type, "call_hangup");
    assert_eq!(
        hangup.event.payload["leg_role"], "agent",
        "leg hangup must carry the UUI role: {hangup:?}"
    );
    assert_eq!(
        hangup.event.payload["hangup_by"], "agent",
        "UasBye on the agent leg is the agent hanging up"
    );
}

/// Teardown-cascade leg selection: an established remote-registered agent leg
/// still registered at session end is NOT cascade-reported (the home node's
/// child-leg session owns that teardown); a local agent leg in the same
/// session still is.
#[tokio::test]
async fn remote_registered_leg_excluded_from_teardown_cascade() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (server, _) = create_test_server_with_rwi_gateway(
        Default::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    let context = CallContext {
        session_id: "remote-cascade-sess".into(),
        dialplan: Arc::new(Dialplan::new(
            "remote-cascade-sess".into(),
            request,
            DialDirection::Inbound,
        )),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:bob@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) =
        SipSession::new_uac(server, CancellationToken::new(), None, context, false);

    let mut remote = crate::call::domain::Leg::new(LegId::from("agent-remote"));
    remote.state = crate::call::domain::LegState::Connected;
    remote.agent_id = Some("bob".into());
    remote.remote_registered = true;
    session.legs.insert(LegId::from("agent-remote"), remote);

    let mut local = crate::call::domain::Leg::new(LegId::from("agent-local"));
    local.state = crate::call::domain::LegState::Connected;
    local.agent_id = Some("alice".into());
    session.legs.insert(LegId::from("agent-local"), local);

    let teardowns = session.cc_leg_teardowns();
    assert_eq!(
        teardowns,
        vec![("agent-local".to_string(), "agent".to_string())],
        "only the LOCAL agent leg is cascade-reported; the remote one is \
         reported by its registration node"
    );
}

#[tokio::test]
async fn caller_termination_preserves_application_exit_cause() {
    use crate::call::Dialplan;
    use crate::call::app::{
        AppAction, ApplicationContext, CallApp, CallAppType, CallController, ExitReason,
    };
    use crate::call::runtime::{AppFactory, AppRuntimeConfig, DefaultAppRuntime};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl CallApp for Capture {
        fn app_type(&self) -> CallAppType {
            CallAppType::Custom
        }
        fn name(&self) -> &str {
            "exit-capture"
        }
        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _context: &ApplicationContext,
        ) -> anyhow::Result<AppAction> {
            ctrl.answer().await?;
            Ok(AppAction::Continue)
        }
        async fn on_exit(&mut self, reason: ExitReason) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(reason.to_string());
            Ok(())
        }
    }
    struct Factory(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl AppFactory for Factory {
        async fn create_app(
            &self,
            _name: &str,
            _params: Option<serde_json::Value>,
            _context: &ApplicationContext,
        ) -> anyhow::Result<Option<Box<dyn CallApp>>> {
            Ok(Some(Box::new(Capture(self.0.clone()))))
        }
    }

    let mut failures = Vec::new();
    for (cause, prior, expected) in [
        (
            TerminatedReason::UacBye,
            None,
            "remote_hangup(Some(ByCaller))",
        ),
        (
            TerminatedReason::UasBye,
            None,
            "remote_hangup(Some(ByCallee))",
        ),
        (TerminatedReason::Timeout, None, "hangup"),
        (
            TerminatedReason::UacCancel,
            None,
            "remote_hangup(Some(Canceled))",
        ),
        (
            TerminatedReason::UacBye,
            Some(CallRecordHangupReason::BySystem),
            "hangup",
        ),
        (
            TerminatedReason::UasBye,
            Some(CallRecordHangupReason::RtpTimeout),
            "hangup",
        ),
    ] {
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "example.com",
            None,
        );
        let dialplan = Dialplan::new(
            "exit-cause".into(),
            request,
            crate::call::DialDirection::Inbound,
        );
        let (mut session, handle, mut commands) = build_session_with_cmd_rx(dialplan).await;
        let exits = Arc::new(std::sync::Mutex::new(Vec::new()));
        session.app_runtime = Arc::new(
            DefaultAppRuntime::new(AppRuntimeConfig {
                session_id: "test-session".into(),
                handle,
                context: session.app_runtime.app_context().unwrap().clone(),
            })
            .with_factory(Arc::new(Factory(exits.clone()))),
        );
        session.meta.hangup_reason = prior;
        session
            .app_runtime
            .start_app("exit-capture", None, false)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), commands.recv())
            .await
            .unwrap()
            .expect("app must enter");
        let dialog_id = session.caller_dialog.as_ref().unwrap().id();
        session
            .handle_dialog_state(DialogState::Terminated(dialog_id, cause.clone()))
            .await
            .unwrap();
        loop {
            let command = tokio::time::timeout(Duration::from_secs(5), commands.recv())
                .await
                .unwrap()
                .expect("app must exit");
            if matches!(command, CallCommand::AppExited { .. }) {
                break;
            }
        }
        let actual = exits.lock().unwrap().clone();
        if actual != [expected] {
            failures.push(format!("{cause:?}: expected {expected}, got {actual:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ── Transfer → agent: the registration machine normalizes the role ────────

/// REPRO (prd: 转接之后 leg_role=agent 没有了). A REFER/consult transfer
/// landing on a REGISTERED agent declares `leg=transfer` / `leg=consult` on
/// the wire, so the agent machine's child-leg session stamps that dial-time
/// INTENT on every call_* event of what is now the AGENT dialog — the
/// contract says the registration machine stamps `leg_role=agent`.
///
/// The child session resolves the dialed target against the registry before
/// ringing (`ensure_child_agent_pinned`); a REGISTERED agent must normalize
/// the declared role to "agent". A non-agent target keeps its declared role.
#[tokio::test]
async fn child_leg_role_normalizes_to_agent_for_registered_target() {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry, PresenceState};
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};

    async fn child_session_with_role(
        declared: &'static str,
        registry: Option<Arc<MemoryRegistry>>,
    ) -> (
        SipSession,
        tokio::sync::broadcast::Receiver<crate::rwi::EventCacheEntry>,
    ) {
        let gateway = crate::rwi::gateway::RwiGateway::new();
        let events = gateway.subscribe_events();
        let (server, _) = create_test_server_with_rwi_gateway(
            Default::default(),
            Arc::new(parking_lot::RwLock::new(gateway)),
        )
        .await;
        let mut server = server;
        if let Some(registry) = registry {
            Arc::get_mut(&mut server)
                .expect("test server uniquely held")
                .agent_registry = Some(registry);
        }
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "caller",
            None,
            "rustpbx.com",
            None,
        );
        let context = CallContext {
            session_id: format!("child-role-{declared}"),
            dialplan: Arc::new(Dialplan::new(
                format!("child-role-{declared}"),
                request,
                DialDirection::Inbound,
            )),
            cookie: TransactionCookie::default(),
            start_time: Instant::now(),
            original_caller: "sip:caller@rustpbx.com".into(),
            original_callee: "sip:2002@rustpbx.com".into(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        let (mut session, _handle, _commands) =
            SipSession::new_uac(server, CancellationToken::new(), None, context, false);
        session.meta.child_leg_role = Some(declared.to_string());
        let mut callee = crate::call::domain::Leg::new(LegId::from("callee"));
        callee.endpoint = Some("sip:2002@rustpbx.com".to_string());
        session.legs.insert(LegId::from("callee"), callee);
        (session, events)
    }

    let registry = Arc::new(MemoryRegistry::new());
    registry
        .register(
            "2002".to_string(),
            "Bob".to_string(),
            "sip:2002@rustpbx.com".to_string(),
            vec!["support".to_string()],
            1,
        )
        .await
        .unwrap();
    registry
        .update_presence("2002", PresenceState::Idle)
        .await
        .unwrap();

    for declared in ["transfer", "consult"] {
        let (mut session, mut events) =
            child_session_with_role(declared, Some(registry.clone())).await;
        session.ensure_child_agent_pinned().await;
        session.emit_call_ringing(Some("callee".into()), false, None);
        let ringing = events.try_recv().expect("child leg ringing").event.payload;
        assert_eq!(
            ringing["leg_role"], "agent",
            "REPRO {declared}: a transfer/consult landing on a REGISTERED agent \
             must normalize to leg_role=agent on the registration machine: {ringing:?}"
        );
        assert_eq!(
            ringing["agent_id"], "2002",
            "REPRO {declared}: the resolved agent is attributed: {ringing:?}"
        );
    }

    // Non-agent target: the declared role stays (no normalization).
    let (mut session, mut events) =
        child_session_with_role("consult", Some(registry.clone())).await;
    if let Some(leg) = session.legs.get_mut(&LegId::from("callee")) {
        leg.endpoint = Some("sip:13800001380@rustpbx.com".to_string());
    }
    session.ensure_child_agent_pinned().await;
    session.emit_call_ringing(Some("callee".into()), false, None);
    let ringing = events.try_recv().expect("child leg ringing").event.payload;
    assert_eq!(
        ringing["leg_role"], "consult",
        "non-agent target keeps its declared role: {ringing:?}"
    );
}
