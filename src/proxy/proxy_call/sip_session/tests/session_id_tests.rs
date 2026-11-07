use super::*;

/// Serializes tests that touch the process-global session-id settings:
/// cargo runs `#[tokio::test]`s on parallel worker threads, so the
/// seed/restore pattern alone is racy (a Native-mode assertion can observe
/// the Rfc7989 seed another test installed).
static SETTINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// 本模块的 settings 依赖用例一律在此串行锁下执行（即 `SETTINGS_LOCK`，
/// 与显式 seed/restore 的用例互斥，避免并行交错读到错误模式）。
fn lock_settings_serial() -> parking_lot::MutexGuard<'static, ()> {
    SETTINGS_LOCK.lock()

#[tokio::test]
async fn session_id_peer_dialog_query_does_not_confuse_correlation_with_control() {
    for (from_peer, role, local_queue) in [
        (false, Some("agent"), false),
        (true, Some("agent"), false),
        (true, Some("consult"), false),
        (true, None, false),
        (true, Some("agent"), true),
    ] {
        let mut session = inherited_delivery_session(
            "aabbccddeeff0011",
            Some("aabbccddeeff0011"),
            true,
            role,
            from_peer,
            false,
        )
        .await;
        if local_queue {
            session.meta.queue_name = Some("locally-owned-queue".into());
        }
        let caller_id = session.caller_dialog.as_ref().unwrap().id().call_id;
        let (reply, result) = tokio::sync::oneshot::channel();
        assert!(
            session
                .execute_command(CallCommand::QueryPeerDialog { reply }, None)
                .await
                .success
        );
        let upstream = result.await.unwrap();
        if from_peer && role.is_some() && !local_queue {
            assert_eq!(
                upstream,
                Some((
                    caller_id,
                    "192.0.2.44:5060".parse().unwrap(),
                    role.unwrap().into()
                ))
            );
        } else {
            assert!(
                upstream.is_none(),
                "correlation and external declarations cannot redirect control"
            );
        }
        assert_eq!(
            session.meta.root_session_id.as_deref(),
            Some("aabbccddeeff0011")
        );
    }
}

/// Settings 全局是进程级的：本模块内有用例改写 `header_mode`（Rfc7989），
/// 有用例断言 Native 行为——并行交错会让后者读到错误模式（间歇红）。
/// 本模块的 settings 依赖用例一律在此串行锁下执行。
static SETTINGS_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn vendor_review_regression_statistics_owner_survives_attribution_reset() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    const ROOT: &str = "11111111111141118111111111111111";
    for (trusted, local_id, expected_owner) in [
        (true, ROOT, false),
        (true, "22222222222242228222222222222222", true),
        (false, ROOT, true),
    ] {
        let gateway = crate::rwi::gateway::RwiGateway::new();
        let (mut server, _) = create_test_server_with_rwi_gateway(
            Default::default(),
            Arc::new(parking_lot::RwLock::new(gateway)),
        )
        .await;
        Arc::get_mut(&mut server).unwrap().cluster_peer_ips = vec!["192.0.2.10".parse().unwrap()];
        let mut request = create_test_request(
            rsipstack::sip::Method::Invite,
            "source",
            None,
            "example.test",
            None,
        );
        request.headers.push(crate::call::uui::build_uui_header(
            ROOT,
            None,
            None,
            None,
            Some("agent"),
        ));
        let (tx, _) = create_transaction(request.clone()).await;
        let (state_tx, _states) = mpsc::unbounded_channel();
        let dialog = server
            .dialog_layer
            .get_or_create_server_invite(&tx, state_tx, None, None)
            .unwrap();
        let cookie = TransactionCookie::default();
        cookie.insert_extension(crate::call::CallerPeerContext(
            if trusted {
                "192.0.2.10:5060"
            } else {
                "192.0.2.99:5060"
            }
            .into(),
        ));
        let context = CallContext {
            session_id: local_id.into(),
            dialplan: Arc::new(Dialplan::new(
                local_id.into(),
                request,
                DialDirection::Inbound,
            )),
            cookie,
            start_time: Instant::now(),
            original_caller: "sip:source@example.test".into(),
            original_callee: "sip:agent@example.test".into(),
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
        assert_eq!(session.session_hook_ctx().owns_statistics, expected_owner);
        // The production blind-transfer boundary clears the previous agent's role.
        session.clear_agent_attribution();
        assert_eq!(
            session.session_hook_ctx().owns_statistics,
            expected_owner,
            "changing the served agent must not change the session's statistics writer",
        );
    }
}

fn lock_settings_serial() -> std::sync::MutexGuard<'static, ()> {
    SETTINGS_TEST_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

async fn inherited_session_id_session(header_id: &str, uui_id: Option<&str>) -> SipSession {
    inherited_session_id_session_with_enabled(header_id, uui_id, true).await
}

async fn inherited_session_id_session_with_enabled(
    header_id: &str,
    uui_id: Option<&str>,
    enabled: bool,
) -> SipSession {
    inherited_delivery_session(header_id, uui_id, enabled, Some("agent"), false, false).await
}

async fn inherited_delivery_session(
    header_id: &str,
    uui_id: Option<&str>,
    enabled: bool,
    role: Option<&str>,
    from_peer: bool,
    use_media_proxy: bool,
) -> SipSession {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (mut server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    Arc::get_mut(&mut server).unwrap().cluster_peer_ips = vec!["192.0.2.44".parse().unwrap()];
    let mut request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "example.test",
        None,
    );
    if !header_id.is_empty() {
        request.headers.push(
            rsipstack::sip::headers::SessionId::new(format!(
                "{header_id};remote={}",
                rsipstack::sip::headers::SessionId::NIL
            ))
            .into(),
        );
    }
    if let Some(id) = uui_id {
        request.headers.push(crate::call::uui::build_uui_header(
            id, None, None, None, role,
        ));
    }
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("server dialog");
    let local_id = "1234567890abcdef1234567890abcdef";
    let dialplan = Dialplan::new(local_id.into(), request, DialDirection::Inbound)
        .with_caller("sip:alice@example.test".parse().unwrap())
        .with_session_id_enabled(enabled);
    let cookie = TransactionCookie::default();
    if from_peer {
        cookie.insert_extension(crate::call::CallerPeerContext("192.0.2.44:5060".into()));
    }
    let context = CallContext {
        session_id: local_id.into(),
        dialplan: Arc::new(dialplan),
        cookie,
        start_time: Instant::now(),
        original_caller: "alice".into(),
        original_callee: "sip:bob@example.test".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        dialog,
        use_media_proxy,
    );
    session
}

#[tokio::test]
async fn session_id_rfc_padding_preserves_original_root_without_truncating_other_ids() {
    let _settings_serial = lock_settings_serial();
    let short = "0b9e6c1e1b404e8f";
    let padded = format!("{short:0>32}");
    // The UUI carries the original root; absent or unrelated UUI must never
    // cause a legitimate leading-zero 32-character ID to be shortened.
    for (uui, expected) in [
        (Some(short), short),
        (None, padded.as_str()),
        (Some("aabbccddeeff0011"), padded.as_str()),
    ] {
        let session = inherited_session_id_session(&padded, uui).await;
        assert_eq!(session.session_hook_ctx().logical_session_id(), expected);
        assert_eq!(
            session.record_snapshot().root_session_id.as_deref(),
            Some(expected)
        );
        let gateway = session.server.rwi_gateway.as_ref().unwrap().read();
        let meta = gateway
            .meta_store
            .get_sync(&session.context.session_id)
            .unwrap();
        assert_eq!(meta.session_id.as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn session_id_native_inherited_length_survives_outgoing_boundary() {
    use crate::call::session_id::{SessionIdHeaderMode, init_settings, settings};
    // 断言 Native 行为（build_wire_header 读进程级 settings）——必须与
    // Rfc7989 改写用例串行，因此显式 seed 而不是依赖环境默认。
    let _guard = SETTINGS_LOCK.lock();
    // Deterministic Native mode regardless of test scheduling order.
    struct RestoreSettings(crate::call::session_id::SessionIdSettings);
    impl Drop for RestoreSettings {
        fn drop(&mut self) {
            crate::call::session_id::init_settings(
                self.0.length,
                self.0.header_mode,
                self.0.inherit_callid,
            );
        }
    }
    let _restore = RestoreSettings(settings());
    init_settings(16, SessionIdHeaderMode::Native, true);
    let short = "0b9e6c1e1b404e8f";
    let session = inherited_session_id_session(short, Some(short)).await;
    assert_eq!(session.session_hook_ctx().logical_session_id(), short);
    let outgoing = session.session_id_for_outgoing_leg();
    assert_eq!(outgoing.as_deref(), Some(short));
    let (option_id, header) = crate::call::session_id::build_wire_header(Some(short));
    assert!(option_id.is_none());
    let Some(rsipstack::sip::Header::SessionId(header)) = header else {
        panic!("native inherited short ID must be transmitted as a raw header");
    };
    assert_eq!(
        crate::call::session_id::parse_wire_header(header.value()).as_deref(),
        Some(short)
    );
}

#[tokio::test]
async fn session_id_rfc_generic_target_preserves_short_root_and_existing_uui() {
    let _settings_serial = lock_settings_serial();
    use crate::call::session_id::{
        SessionIdHeaderMode, SessionIdSettings, init_settings, settings,
    };

    struct RestoreSettings(SessionIdSettings);
    impl Drop for RestoreSettings {
        fn drop(&mut self) {
            init_settings(self.0.length, self.0.header_mode, self.0.inherit_callid);
        }
    }
    // The production wire builder reads process settings. Held under
    // SETTINGS_LOCK together with the other settings-dependent tests, and
    // restored even on panic.
    let _guard = SETTINGS_LOCK.lock();
    let _restore = RestoreSettings(settings());
    init_settings(16, SessionIdHeaderMode::Rfc7989, true);
    let short = "0b9e6c1e1b404e8f";
    let supplied_uui = crate::call::uui::build_uui_header(
        short,
        Some("sales"),
        Some("Sales"),
        Some("sales-group"),
        None,
    );
    let unrelated_uui = rsipstack::sip::Header::Other(
        crate::call::uui::UUI_HEADER_NAME.into(),
        "external-data;encoding=ascii;purpose=isdn-uui".into(),
    );
    for (enabled, supplied) in [
        (true, None),
        (true, Some(&supplied_uui)),
        (true, Some(&unrelated_uui)),
        (false, None),
    ] {
        let mut session =
            inherited_session_id_session_with_enabled(short, Some(short), enabled).await;
        let target = crate::call::Location {
            aor: "sip:bob@example.test".parse().unwrap(),
            headers: supplied.map(|header| vec![header.clone()]),
            ..Default::default()
        };
        let (option, _, _, _) = session
            .build_target_invite_option(&target, None, None)
            .await
            .expect("generic target INVITE");
        let headers = option.headers.unwrap_or_default();
        let uui_headers: Vec<_> = headers
            .iter()
            .filter(|header| {
                header
                    .name()
                    .eq_ignore_ascii_case(crate::call::uui::UUI_HEADER_NAME)
            })
            .collect();
        if !enabled {
            assert!(option.session_id.is_none());
            assert!(uui_headers.is_empty(), "plain P2P must remain header-free");
            continue;
        }
        let wire_id = option.session_id.expect("RFC wire ID");
        assert_eq!(wire_id, format!("{short:0>32}"));
        let uui = uui_headers
            .iter()
            .find_map(|header| crate::call::uui::parse_uui(&header.value()));
        let receiver = inherited_session_id_session(
            &wire_id,
            uui.as_ref().map(|value| value.session_id.as_str()),
        )
        .await;
        assert_eq!(receiver.session_hook_ctx().logical_session_id(), short);
        let cc_count = uui_headers
            .iter()
            .filter(|header| crate::call::uui::parse_uui(&header.value()).is_some())
            .count();
        assert_eq!(cc_count, 1, "exactly one root correlation UUI");
        if let Some(supplied) = supplied {
            assert!(
                uui_headers
                    .iter()
                    .any(|header| header.value() == supplied.value())
            );
            assert_eq!(
                uui_headers.len(),
                if supplied == &unrelated_uui { 2 } else { 1 }
            );
        } else {
            assert_eq!(uui_headers.len(), 1);
        }
    }
}

/// Both older peers (root UUI without a role) and new peers carry one root
/// through hook context, actual delivered events, and the CDR snapshot.
#[tokio::test]
async fn session_id_peer_delivery_preserves_root_without_collapsing_local_ids() {
    let root = "0b9e6c1e1b404e8f";
    for wire_header in [root, ""] {
        for role in [None, Some("agent")] {
            for from_peer in [false, true] {
                let mut session = inherited_delivery_session(
                    wire_header,
                    Some(root),
                    true,
                    role,
                    from_peer,
                    true,
                )
                .await;
                let mut events = session
                    .server
                    .rwi_gateway
                    .as_ref()
                    .unwrap()
                    .read()
                    .subscribe_events();
                assert_eq!(session.session_hook_ctx().logical_session_id(), root);
                assert_ne!(
                    session.context.session_id, root,
                    "local leg identity must remain unique"
                );
                assert_eq!(
                    session.meta.child_leg_role.as_deref(),
                    if from_peer { role } else { None }
                );
                assert_eq!(
                    session.record_snapshot().root_session_id.as_deref(),
                    Some(root)
                );
                // Exercise actual recording commands and CDR publication, not
                // only generation of a recording identity or configuration.
                use crate::proxy::tests::test_sip_session_regressions::setup_recording_test_media;
                use rustrtc::peer_connection::RtpObserver;
                use rustrtc::rtp::{RtpHeader, RtpPacket};
                setup_recording_test_media(&mut session).await;
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("peer-transfer.wav");
                let recording_id = "33333333-3333-4333-8333-333333333333";
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
                let caller = session.media_leg(&LegId::from("caller")).unwrap();
                let tap = caller.ingress_tap();
                let mut encoder = audio_codec::create_encoder(audio_codec::CodecType::PCMU);
                for seq in 0..10u16 {
                    let packet = RtpPacket::new(
                        RtpHeader::new(0, seq, u32::from(seq) * 160, 1234),
                        encoder.encode(&vec![8000; 160]).to_vec(),
                    );
                    tap.on_ingress(&packet, "127.0.0.1:12345".parse().unwrap());
                }
                let (records, mut received) = mpsc::channel(1);
                session.reporter = Some(CallReporter {
                    server: session.server.clone(),
                    context: session.context.clone(),
                    call_record_sender: Some(records),
                });
                session.cleanup().await;
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
                            .any(|sample| i16::from_le_bytes([sample[0], sample[1]]) != 0)
                );
                let record = received.try_recv().expect("CDR must be published");
                assert_eq!(record.session_id.as_deref(), Some(root));
                assert_eq!(record.recorder.len(), 1);
                assert_eq!(record.recorder[0].unique_id.as_deref(), Some(recording_id));
                assert_eq!(
                    record.recorder[0].extra.as_ref().unwrap()["session_id"],
                    root
                );
                let mut hangups = Vec::new();
                while let Ok(entry) = events.try_recv() {
                    if entry.event.event_type == "call_hangup" {
                        hangups.push(entry.event);
                    }
                }
                assert!(!hangups.is_empty());
                for event in hangups {
                    assert_eq!(event.payload["session_id"], root, "{event:?}");
                    assert_eq!(
                        event.call_id.as_deref(),
                        Some("1234567890abcdef1234567890abcdef")
                    );
                }
            }
        }
    }
}
