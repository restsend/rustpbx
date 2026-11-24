use super::*;

#[tokio::test]
async fn attended_commit_on_home_dialog_reports_the_retained_agent_once() {
    use crate::call::session_id::{CONSULT_PARENT_HEADER, CONSULT_SESSION_HEADER};
    use crate::proxy::tests::common::create_test_request;
    use rsipstack::sip::{Header, Method};

    let mut invite = create_test_request(Method::Invite, "dispatcher", None, "example.test", None);
    invite.headers.push(crate::call::uui::build_uui_header(
        "aabbccddeeff0011", None, None, None, Some("agent"),
    ));
    invite.headers.push(Header::Other(CONSULT_PARENT_HEADER.into(), "root-call".into()));
    invite.headers.push(Header::Other(CONSULT_SESSION_HEADER.into(), "consult-call".into()));
    mark_consultation_data_join(&mut invite, 1);
    let mut session = delivery_session_from_request(
        invite, Some("consult-call"), true, true, false, false, false,
    ).await;
    let callee = LegId::from("callee");
    let mut leg = Leg::new(callee.clone());
    leg.agent_id = Some("agent-b".into());
    session.legs.insert(callee.clone(), leg);
    let gateway = session.server.rwi_gateway.as_ref().unwrap().clone();
    let origin_recording = "12345678-1234-4123-8123-123456789abc";
    let retained_recording = "87654321-4321-4321-8321-cba987654321";
    gateway.write().set_recording_identity_lookup(Some(Arc::new(
        move |call_id, agent_id, _| match (call_id, agent_id) {
            ("root-call", "agent-a") => Some(origin_recording.into()),
            ("consult-call", "agent-b") => Some(retained_recording.into()),
            _ => None,
        },
    )));
    let mut events = gateway.read().subscribe_events();
    let dialog = session.caller_dialog.as_ref().unwrap().id();
    session.update_leg_state(&callee, LegState::Connected);
    let answered = events.try_recv().unwrap().event;
    assert_eq!(answered.event_type, "call_answered");
    assert_eq!(answered.payload["consultation_peer_dialog_id"], dialog.call_id);

    for (root, marker, expected) in [
        (None, true, false),
        (Some("root-call"), false, false),
        (Some("unrelated-root"), true, false),
        (Some("root-call"), true, true),
        (Some("root-call"), true, false),
    ] {
        let mut update = create_test_request(Method::Update, "dispatcher", None, "example.test", None);
        if marker {
            update.headers.push(Header::UserToUser("consult-state=talk".into()));
        }
        if let Some(root) = root {
            let original = r#"{"caller_name":"customer-number","callee_name":"original-access"}"#;
            update.headers.push(Header::Other(
                "X-CC-Attended-Commit".into(),
                format!("{root};consult-call;inbound;agent-a;{original}"),
            ));
        }
        let (transaction, mut responses) = TransactionHandle::new();
        // No handset dialog is needed to observe the business event. A failed
        // handset relay does not roll back the already committed room.
        let _ = session.handle_dialog_state(DialogState::Updated(
            dialog.clone(), update, transaction,
        )).await;
        let response = responses.try_recv().ok();
        if let Some(root) = root {
            let rsipstack::dialog::dialog::TransactionCommand::Respond { status, headers, .. } =
                response.expect("a commit request requires a PBX response even without a handset");
            if root == "root-call" && marker {
                assert_eq!(status, StatusCode::OK);
                assert!(headers.unwrap_or_default().is_empty());
            } else {
                assert_eq!(status, StatusCode::Conflict);
            }
        } else {
            assert!(response.is_none(), "plain talk cannot produce a PBX commit acknowledgement");
        }
        let retained = events.try_recv().ok().map(|entry| entry.event);
        assert_eq!(retained.is_some(), expected, "commit root: {root:?}");
        if let Some(retained) = retained {
            assert_eq!(retained.event_type, "call_transfer_retained");
            assert_eq!(retained.call_id.as_deref(), Some("consult-call"));
            assert_eq!(retained.payload["session_id"], "consult-call");
            assert_eq!(retained.payload["agent_id"], "agent-b");
            assert_eq!(retained.payload["retained_transfer"]["root_session_id"], "root-call");
            assert_eq!(retained.payload["retained_transfer"]["root"]["caller_name"], "customer-number");
            assert_eq!(retained.payload["transfer_source"]["agent_id"], "agent-a");
            assert_eq!(retained.payload["recording_id"], retained_recording);
            assert_eq!(retained.payload["consultation_peer_dialog_id"], dialog.call_id);
        }
    }

    // The recorder releases its reservation before the final hangup; its
    // agent-scoped metadata is still available to the gateway at dispatch.
    {
        let mut gateway = gateway.write();
        gateway.set_recording_identity_lookup(None);
        let mut meta = gateway
            .meta_store
            .get_sync("consult-call")
            .unwrap_or_default();
        meta.agent_id = Some("agent-b".into());
        meta.recording_id = Some(retained_recording.into());
        gateway.meta_store.insert("consult-call".into(), meta);
    }
    let replay = gateway.read().get_events_for_call(&"consult-call".into());
    let retained = replay
        .iter()
        .find(|entry| entry.event.event_type == "call_transfer_retained")
        .unwrap();
    assert_eq!(retained.event.payload["recording_id"], retained_recording);

    session.cleanup().await;
    let mut retained_releases = Vec::new();
    while let Ok(entry) = events.try_recv() {
        if entry.event.event_type == "call_hangup" && entry.event.payload["leg_role"] == "agent" {
            retained_releases.push(entry.event.payload);
        }
    }
    assert_eq!(retained_releases.len(), 1);
    assert_eq!(retained_releases[0]["retained_transfer"]["root_session_id"], "root-call");
    assert_eq!(retained_releases[0]["retained_transfer"]["direction"], "inbound");
    assert_eq!(retained_releases[0]["retained_transfer"]["root"]["callee_name"], "original-access");
    assert_eq!(retained_releases[0]["recording_id"], retained_recording);
    assert_eq!(retained_releases[0]["consultation_peer_dialog_id"], dialog.call_id);
}

fn mark_consultation_data_join(invite: &mut rsipstack::sip::Request, count: usize) {
    use crate::call::session_id::CONSULT_DATA_JOIN_PARAM;
    use rsipstack::sip::{Header, Param};
    for header in invite.headers.iter_mut() {
        if let Header::From(from) = header {
            let mut from = from.typed().unwrap();
            from.params.extend((0..count).map(|_| Param::Other(CONSULT_DATA_JOIN_PARAM.into(), None)));
            *header = from.into();
            return;
        }
    }
    panic!("expected a generated From header");
}

#[tokio::test]
async fn attended_consultation_data_join_requires_one_marker_from_trusted_peer() {
    use crate::call::session_id::{CONSULT_PARENT_HEADER, CONSULT_SESSION_HEADER};
    use crate::proxy::tests::common::create_test_request;
    use rsipstack::sip::{Header, Method};

    for (from_peer, marker_count, expected) in [
        (true, 0, false), (true, 1, true), (true, 2, false), (false, 1, false),
    ] {
        let mut invite = create_test_request(Method::Invite, "dispatcher", None, "example.test", None);
        invite.headers.push(crate::call::uui::build_uui_header(
            "aabbccddeeff0011", None, None, None, Some("agent"),
        ));
        invite.headers.push(Header::Other(CONSULT_PARENT_HEADER.into(), "root-call".into()));
        invite.headers.push(Header::Other(CONSULT_SESSION_HEADER.into(), "consult-call".into()));
        // A location-supplied arbitrary extension header may pass through an
        // old dispatcher, and cannot opt in without a generated From marker.
        invite.headers.push(Header::Other("X-CC-Consult-Data-Join".into(), "1".into()));
        mark_consultation_data_join(&mut invite, marker_count);
        let mut session = delivery_session_from_request(
            invite, Some("collided-call"), true, from_peer, false, false, false,
        ).await;
        let dialog = session.caller_dialog.as_ref().unwrap().id().call_id;
        let gateway = session.server.rwi_gateway.as_ref().unwrap().clone();
        let mut events = gateway.read().subscribe_events();
        let callee = LegId::from("callee");
        session.legs.insert(callee.clone(), Leg::new(callee.clone()));
        session.update_leg_state(&callee, LegState::Connected);
        let event = events.try_recv().unwrap().event;
        assert_eq!(event.event_type, "call_answered");
        assert_eq!(event.call_id.as_deref(), Some("collided-call"));
        assert_eq!(
            event.payload.get("consultation_peer_dialog_id").and_then(serde_json::Value::as_str),
            expected.then_some(dialog.as_str()),
        );
    }
}

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

/// settings 依赖用例一律在 crate 级共享串行锁下执行（见 lock_settings_serial）。

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

/// Settings 全局是进程级的：本模块内有用例改写 `header_mode`（Rfc7989），
/// 有用例断言 Native 行为——并行交错会让后者读到错误模式（间歇红）。
/// settings 依赖用例一律在 crate 级共享串行锁（`session_id::settings_test_lock`，
/// 与 `call::session_id` / proxy `call.rs` 的 settings 用例同一把锁）下执行。
fn lock_settings_serial() -> std::sync::MutexGuard<'static, ()> {
    crate::call::session_id::settings_test_lock()
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
    inherited_delivery_session_opts(
        header_id,
        uui_id,
        enabled,
        role,
        from_peer,
        use_media_proxy,
        false,
        false,
    )
    .await
}

/// Full-option variant: `device_custom_roots` flips
/// `proxy.session_id_device_custom_root` on the test server; `from_trunk`
/// inserts a `TrunkContext` (trunk-sourced INVITE).
#[allow(clippy::too_many_arguments)]
async fn inherited_delivery_session_opts(
    header_id: &str,
    uui_id: Option<&str>,
    enabled: bool,
    role: Option<&str>,
    from_peer: bool,
    use_media_proxy: bool,
    device_custom_roots: bool,
    from_trunk: bool,
) -> SipSession {
    use crate::proxy::tests::common::create_test_request;

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
    delivery_session_from_request(
        request,
        Some("1234567890abcdef1234567890abcdef"),
        enabled,
        from_peer,
        use_media_proxy,
        device_custom_roots,
        from_trunk,
    )
    .await
}

async fn delivery_session_from_request(
    request: rsipstack::sip::Request,
    local_id: Option<&str>,
    enabled: bool,
    from_peer: bool,
    use_media_proxy: bool,
    device_custom_roots: bool,
    from_trunk: bool,
) -> SipSession {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{create_test_server_with_rwi_gateway, create_transaction};

    let mut proxy_config = ProxyConfig::default();
    proxy_config.session_id_device_custom_root = device_custom_roots;
    let (mut server, _) = create_test_server_with_rwi_gateway(
        proxy_config,
        Arc::new(parking_lot::RwLock::new(crate::rwi::gateway::RwiGateway::new())),
    )
    .await;
    Arc::get_mut(&mut server).unwrap().cluster_peer_ips = vec!["192.0.2.44".parse().unwrap()];
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("server dialog");
    let cookie = TransactionCookie::default();
    if from_peer {
        cookie.insert_extension(crate::call::CallerPeerContext("192.0.2.44:5060".into()));
    }
    if from_trunk {
        cookie.insert_extension(crate::call::cookie::TrunkContext {
            id: None,
            name: "customer-x".to_string(),
            did_numbers: vec![],
        });
    }
    let local_id = local_id.map(str::to_owned).unwrap_or_else(|| {
        crate::proxy::call::CallModule::resolve_inbound_session_id(
            &server,
            &cookie,
            &request,
            &dialog.id().call_id,
            enabled,
        )
    });
    let dialplan = Dialplan::new(local_id.clone(), request, DialDirection::Inbound)
        .with_caller("sip:alice@example.test".parse().unwrap())
        .with_session_id_enabled(enabled);
    let context = CallContext {
        session_id: local_id,
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

/// Self-contained builder for the trunk correlation-header tests: a session
/// whose dialplan carries `session_id` + an optional `OutboundTrunkContext`,
/// with the trunk config installed in the server's data context.
async fn trunk_correlation_session(with_trunk_context: bool) -> SipSession {
    use crate::call::{DialDirection, Dialplan, OutboundTrunkContext, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::TrunkConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    server.data_context.trunks.insert(
        "customer-x".to_string(),
        TrunkConfig {
            session_id_header: Some("X-Correlation-Id".to_string()),
            ..Default::default()
        },
    );
    server.data_context.trunks.insert(
        "target-g".to_string(),
        TrunkConfig {
            dest: "sip:1.2.3.4:5060".to_string(),
            session_id_header: Some("X-RustPBX-SessionID".to_string()),
            ..Default::default()
        },
    );
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "example.test",
        None,
    );
    let (tx, _) = create_transaction(request.clone()).await;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let dialog = server
        .dialog_layer
        .get_or_create_server_invite(&tx, state_tx, None, None)
        .expect("server dialog");
    let root = "ORD-88923";
    let mut dialplan = Dialplan::new(root.to_string(), request, DialDirection::Outbound)
        .with_caller("sip:alice@example.test".parse().unwrap())
        .with_session_id_enabled(true);
    if with_trunk_context {
        dialplan.extensions.insert(OutboundTrunkContext {
            id: Some(1),
            name: "customer-x".to_string(),
            dest: None,
            tenant_id: None,
        });
    }
    let context = CallContext {
        session_id: root.to_string(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "alice".into(),
        original_callee: "sip:13800000000@1.2.3.4".into(),
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
        false,
    );
    session
}

fn correlation_header_of(
    option: &rsipstack::dialog::invitation::InviteOption,
) -> Option<String> {
    option
        .headers
        .as_ref()?
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case("X-Correlation-Id"))
        .map(|header| header.value().to_string())
}

fn header_value_of(
    option: &rsipstack::dialog::invitation::InviteOption,
    name: &str,
) -> Option<String> {
    option
        .headers
        .as_ref()?
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case(name))
        .map(|header| header.value().to_string())
}

fn header_count_of(
    option: &rsipstack::dialog::invitation::InviteOption,
    name: &str,
) -> usize {
    option
        .headers
        .as_ref()
        .map(|headers| {
            headers
                .iter()
                .filter(|header| header.name().eq_ignore_ascii_case(name))
                .count()
        })
        .unwrap_or(0)
}

/// A CLUSTER PEER dispatching a child leg whose root is a CUSTOM correlation
/// string (non-hex): the constructor's root linkage must accept the UUI data
/// from the peer-trusted seam, so the agent-home session keeps one id across
/// the logical call (`meta.session_id` = root). Untrusted sources stay
/// hex-only — a non-peer INVITE with the same UUI must NOT link.
#[tokio::test]
async fn cluster_child_root_linkage_accepts_non_hex_custom_root() {
    for (from_peer, expected) in [(true, Some("ORD-88923")), (false, None)] {
        let session = inherited_delivery_session(
            "", // no standard `Session-ID` header — build_wire_header refuses non-hex
            Some("ORD-88923"),
            true,
            Some("agent"),
            from_peer,
            false,
        )
        .await;
        assert_eq!(
            session.meta.root_session_id.as_deref(),
            expected,
            "peer UUI data with a non-hex root links; untrusted sources do not"
        );
    }
}

/// LAN device (non-peer, non-trunk) CC leg carrying a CUSTOM (non-hex) UUI
/// data part: with `session_id_device_custom_root` opted in the data part
/// links as the logical root. Default-off, trunk-sourced, unknown-leg and
/// illegal-shape forms stay unlinked (the entropy gate holds).
#[tokio::test]
async fn device_custom_root_linkage_is_opt_in() {
    // 开启 + LAN 设备 consult 腿：custom root 链接。
    let session = inherited_delivery_session_opts(
        "", // 非 hex 不走标准 `Session-ID` 头通道（build_wire_header 拒非 hex）
        Some("ORD-88923"),
        true,
        Some("consult"),
        false,
        false,
        true,
        false,
    )
    .await;
    assert_eq!(
        session.meta.root_session_id.as_deref(),
        Some("ORD-88923"),
        "opted-in device CC leg links the custom correlation root"
    );
    assert_eq!(
        session.meta.dial_leg_role.as_deref(),
        Some("consult"),
        "device legs keep the stats-marker dial intent"
    );

    // 默认关（其余同参）：不链接。
    let session = inherited_delivery_session_opts(
        "",
        Some("ORD-88923"),
        true,
        Some("consult"),
        false,
        false,
        false,
        false,
    )
    .await;
    assert_eq!(
        session.meta.root_session_id.as_deref(),
        None,
        "without the opt-in the device data part must not root the call"
    );

    // trunk 来源：开启也不链接（外部可猜串保持熵门）。
    let session = inherited_delivery_session_opts(
        "",
        Some("ORD-88923"),
        true,
        Some("consult"),
        false,
        false,
        true,
        true,
    )
    .await;
    assert_eq!(
        session.meta.root_session_id.as_deref(),
        None,
        "trunk-sourced INVITEs never use the device custom-root channel"
    );

    // 未知 leg=：不链接。
    let session = inherited_delivery_session_opts(
        "",
        Some("ORD-88923"),
        true,
        Some("weird"),
        false,
        false,
        true,
        false,
    )
    .await;
    assert_eq!(
        session.meta.root_session_id.as_deref(),
        None,
        "root linkage is a CC-leg concept — unknown leg intents never link"
    );

    // 非法形态 data part（'=' 过不了 normalize_custom）：不链接。
    let session = inherited_delivery_session_opts(
        "",
        Some("bad=root"),
        true,
        Some("consult"),
        false,
        false,
        true,
        false,
    )
    .await;
    assert_eq!(
        session.meta.root_session_id.as_deref(),
        None,
        "wire-unsafe data parts are skipped even under the opt-in"
    );
}

/// Outbound round trip: a leg routed to a trunk with `session_id_header`
/// stamps that header with the current session id — the verbatim string the
/// trunk sent on ingress comes back on egress.
#[tokio::test]
async fn trunk_leg_stamps_correlation_header_outbound() {
    let mut session = trunk_correlation_session(true).await;
    let target = crate::call::Location {
        aor: "sip:13800000000@1.2.3.4".parse().unwrap(),
        ..Default::default()
    };
    let (option, _callee, _call_id, _via_home) = session
        .build_target_invite_option(&target, None, None)
        .await
        .expect("trunk-bound INVITE must build");
    assert_eq!(
        correlation_header_of(&option),
        Some("ORD-88923".to_string()),
        "trunk-bound legs stamp the configured correlation header verbatim"
    );
}

/// Internal legs (no `OutboundTrunkContext`) never stamp the correlation
/// header — only trunk-bound legs do.
#[tokio::test]
async fn internal_legs_do_not_stamp_correlation_header() {
    let mut session = trunk_correlation_session(false).await;
    let target = crate::call::Location {
        aor: "sip:1001@example.test".parse().unwrap(),
        ..Default::default()
    };
    let (option, _callee, _call_id, _via_home) = session
        .build_target_invite_option(&target, None, None)
        .await
        .expect("internal INVITE must build");
    assert_eq!(
        correlation_header_of(&option),
        None,
        "internal legs must not stamp the trunk correlation header"
    );
}

/// A route-selected TARGET trunk wins over the trunk this session entered
/// on: blind/consult transfers out a different trunk must carry THAT trunk's
/// header, not the ingress trunk's.
#[tokio::test]
async fn target_trunk_correlation_header_wins_over_dialplan_trunk() {
    let mut session = trunk_correlation_session(true).await;
    let target = crate::call::Location {
        aor: "sip:13800000000@1.2.3.4:5060".parse().unwrap(),
        outbound_trunk: Some(crate::call::OutboundTrunkContext {
            id: Some(2),
            name: "target-g".to_string(),
            dest: Some("sip:1.2.3.4:5060".to_string()),
            tenant_id: None,
        }),
        ..Default::default()
    };
    let (option, _callee, _call_id, _via_home) = session
        .build_target_invite_option(&target, None, None)
        .await
        .expect("target-trunk INVITE must build");
    assert_eq!(
        header_value_of(&option, "X-RustPBX-SessionID").as_deref(),
        Some("ORD-88923")
    );
    assert_eq!(
        correlation_header_of(&option),
        None,
        "the ingress trunk's header must not be stamped on a different target trunk"
    );
}

/// Dynamic legs / direct dials without routing hints fall back to matching
/// the destination host:port against configured trunk next-hops.
#[tokio::test]
async fn destination_matching_stamps_trunk_correlation_header() {
    let mut session = trunk_correlation_session(false).await;
    let target = crate::call::Location {
        aor: "sip:13800000000@1.2.3.4:5060".parse().unwrap(),
        ..Default::default()
    };
    let (option, _callee, _call_id, _via_home) = session
        .build_target_invite_option(&target, None, None)
        .await
        .expect("destination-matched INVITE must build");
    assert_eq!(
        header_value_of(&option, "X-RustPBX-SessionID").as_deref(),
        Some("ORD-88923")
    );
}

/// A stale same-name header (header passthrough / route rewrite / CTI
/// headers) must be replaced by the logical-call root, not left in place or
/// duplicated.
#[tokio::test]
async fn root_id_replaces_stale_correlation_header() {
    let mut session = trunk_correlation_session(true).await;
    let target = crate::call::Location {
        aor: "sip:13800000000@1.2.3.4".parse().unwrap(),
        headers: Some(vec![rsipstack::sip::Header::Other(
            "X-Correlation-Id".to_string(),
            "stale-id".to_string(),
        )]),
        ..Default::default()
    };
    let (option, _callee, _call_id, _via_home) = session
        .build_target_invite_option(&target, None, None)
        .await
        .expect("trunk INVITE must build");
    assert_eq!(
        correlation_header_of(&option).as_deref(),
        Some("ORD-88923"),
        "the root id must win over a stale forwarded header"
    );
    assert_eq!(
        header_count_of(&option, "X-Correlation-Id"),
        1,
        "exactly one correlation header must be sent"
    );
}

/// The dynamic-leg INVITE builder (`initiate_sip_leg` — leg_add / consult /
/// customer transfer / queue dispatch) must carry the same header on the
/// wire, not only in the plan-level option.
#[tokio::test]
async fn dynamic_leg_wire_invite_stamps_root_correlation_header() {
    assert_dynamic_leg_wire_correlation(true).await;
}

#[tokio::test]
async fn dynamic_external_direct_leg_wire_stamps_global_correlation_header() {
    let _serial = lock_settings_serial();
    let _restore = CorrelationSettingsRestore(crate::call::session_id::settings());
    assert_dynamic_leg_wire_correlation(false).await;
}

struct CorrelationSettingsRestore(crate::call::session_id::SessionIdSettings);
impl Drop for CorrelationSettingsRestore {
    fn drop(&mut self) {
        let s = &self.0;
        crate::call::session_id::init_settings(
            s.length,
            s.header_mode,
            s.inherit_callid,
            s.source_header.clone(),
        );
    }
}

async fn assert_dynamic_leg_wire_correlation(explicit_trunk: bool) {
    use crate::call::{DialDirection, Dialplan, OutboundTrunkContext};
    use crate::config::ProxyConfig;
    use crate::proxy::routing::TrunkConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway,
    };
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx_on;
    use std::time::Duration;

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (mut server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    Arc::get_mut(&mut server)
        .unwrap()
        .data_context
        .trunks
        .insert(
            "target-g".to_string(),
            TrunkConfig {
                session_id_header: Some("X-RustPBX-SessionID".to_string()),
                ..Default::default()
            },
        );
    if !explicit_trunk {
        server.data_context.trunks.clear();
        server
            .data_context
            .trunks
            .insert("fromsbc".into(), TrunkConfig::default());
        let s = crate::call::session_id::settings();
        crate::call::session_id::init_settings(
            s.length,
            s.header_mode,
            s.inherit_callid,
            Some("X-RustPBX-SessionID".into()),
        );
    }
    // The shared test server installs a mock (channel) transport. Replace it
    // with a real UDP transport and serve the endpoint so the dynamic leg's
    // INVITE actually reaches the test socket.
    let endpoint = server.endpoint.inner.clone();
    server.endpoint.inner.transport_layer.del_transport(&SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: "127.0.0.1:5060".try_into().unwrap(),
    });
    let transport =
        rsipstack::transport::udp::UdpConnection::create_connection(
            "127.0.0.1:0".parse().unwrap(),
            None,
            None,
        )
        .await
        .unwrap();
    server
        .endpoint
        .inner
        .transport_layer
        .add_transport(transport.into());
    crate::utils::spawn(async move {
        let _ = endpoint.serve().await;
    });

    let root = "708fed201de7cbb";
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "example.test",
        None,
    );
    let dialplan = Dialplan::new(root.to_string(), request, DialDirection::Outbound)
        .with_caller("sip:alice@example.test".parse().unwrap())
        .with_session_id_enabled(true);
    let (mut session, _handle, _commands) = build_session_with_cmd_rx_on(server, dialplan).await;

    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let target = crate::call::Location {
        aor: format!("sip:agent@{address}").parse().unwrap(),
        outbound_trunk: explicit_trunk.then(|| OutboundTrunkContext {
            id: Some(1), name: "target-g".to_string(), dest: None, tenant_id: None,
        }),
        ..Default::default()
    };
    let leg = LegId::from("dynamic-g");
    session
        .legs
        .insert(leg.clone(), crate::call::domain::Leg::new(leg.clone()));
    session
        .initiate_sip_leg(&leg, target, None)
        .await
        .expect("dynamic leg must start");

    let mut buffer = [0u8; 8192];
    let (size, _) = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buffer))
        .await
        .expect("dynamic-leg INVITE must reach the target")
        .expect("UDP receive");
    let invite = String::from_utf8_lossy(&buffer[..size]).to_ascii_lowercase();
    assert!(
        invite.contains(&format!("\r\nx-rustpbx-sessionid: {root}")),
        "dynamic-leg INVITE must carry the root id: {invite}"
    );
    session.cancel_token.cancel();
}

#[tokio::test]
async fn session_id_rfc_padding_preserves_original_root_without_truncating_other_ids() {    let _settings_serial = lock_settings_serial();
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
    // 断言 Native 行为（build_wire_header 读进程级 settings）——必须与
    // Rfc7989 改写用例串行。
    let _settings_serial = lock_settings_serial();
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
            init_settings(
                self.0.length,
                self.0.header_mode,
                self.0.inherit_callid,
                self.0.source_header.clone(),
            );
        }
    }
    // The production wire builder reads process settings. This module is run
    // serially with other settings-dependent tests, and restores even on panic.
    let _restore = RestoreSettings(settings());
    init_settings(16, SessionIdHeaderMode::Rfc7989, true, None);
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

#[tokio::test]
async fn consultation_peer_wire_preserves_routing_and_carries_trusted_business_ids() {
    use crate::proxy::tests::common::create_test_request;
    use rsipstack::sip::{
        Header, Method,
        headers::{CallId, SessionId},
    };

    const PARENT: &str = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
    const PARENT_HEADER: &str = "X-CC-Consult-Parent";
    const CHILD_HEADER: &str = "X-CC-Consult-Session";
    let mut origin = inherited_session_id_session(PARENT, Some(PARENT)).await;
    let child = origin.context.session_id.clone();
    origin.meta.consult_parent_session_id = Some(PARENT.into());
    let callee = LegId::from("callee");
    let mut leg = Leg::new(callee.clone());
    leg.purpose = Some(crate::call::domain::LegPurpose::Agent);
    origin.legs.insert(callee.clone(), leg);
    let target_aor: rsipstack::sip::Uri = "sip:bob@example.test".parse().unwrap();
    let target = crate::call::Location {
        aor: target_aor.clone(),
        registered_aor: Some(target_aor),
        home_proxy: Some(SipAddr::new(
            rsipstack::sip::Transport::Udp,
            rsipstack::sip::HostWithPort::try_from("192.0.2.44:5060").unwrap(),
        )),
        ..Default::default()
    };
    let (option, _, dialog_id, via_peer) = origin
        .build_target_invite_option(&target, None, None)
        .await
        .expect("consultation INVITE");
    assert!(via_peer);
    assert_eq!(option.session_id.as_deref(), Some(PARENT));
    let uui = header_value_of(&option, crate::call::uui::UUI_HEADER_NAME)
        .and_then(|value| crate::call::uui::parse_uui(&value))
        .expect("SIP UUI agent marker");
    assert_eq!(uui.session_id, PARENT);
    assert_eq!(uui.leg.as_deref(), Some("agent"));
    assert_eq!(
        header_value_of(&option, PARENT_HEADER).as_deref(),
        Some(PARENT)
    );
    assert_eq!(
        header_value_of(&option, CHILD_HEADER).as_deref(),
        Some(child.as_str())
    );

    for (trusted, with_hints) in [(true, true), (true, false), (false, true)] {
        let mut request = create_test_request(Method::Invite, "alice", None, "example.test", None);
        request.uri = option.callee.clone();
        request
            .headers
            .0
            .retain(|header| !header.name().eq_ignore_ascii_case("Call-ID"));
        request.headers.push(CallId::new(dialog_id.clone()).into());
        request.headers.push(Header::from(SessionId::new(format!(
            "{};remote={}",
            option.session_id.as_deref().unwrap(),
            SessionId::NIL,
        ))));
        request.headers.extend(
            option
                .headers
                .as_ref()
                .unwrap()
                .iter()
                .filter(|header| {
                    with_hints
                        || ![PARENT_HEADER, CHILD_HEADER]
                            .iter()
                            .any(|name| header.name().eq_ignore_ascii_case(name))
                })
                .cloned()
                .collect(),
        );
        let mut receiver = delivery_session_from_request(request, None, true, trusted, false, false, false).await;
        let expected_parent = (trusted && with_hints).then_some(PARENT);
        let expected_consult = (trusted && with_hints).then_some(child.as_str());
        let ctx = receiver.session_hook_ctx();
        assert_eq!(ctx.logical_session_id(), PARENT);
        assert_eq!(ctx.owns_statistics, !trusted);
        if trusted {
            assert_eq!(receiver.context.session_id, PARENT);
        } else {
            assert_ne!(receiver.context.session_id, PARENT);
            assert_ne!(receiver.context.session_id, child);
        }
        let mut events = receiver
            .server
            .rwi_gateway
            .as_ref()
            .unwrap()
            .read()
            .subscribe_events();
        receiver.legs.insert(callee.clone(), Leg::new(callee.clone()));
        assert!(receiver.update_leg_state(&callee, LegState::Ringing));
        let event = events.try_recv().expect("callee ringing event").event;
        assert_eq!(event.event_type, "call_ringing");
        assert_eq!(
            event.call_id.as_deref(),
            Some(receiver.context.session_id.as_str())
        );
        assert_eq!(event.payload["session_id"], PARENT);
        assert_eq!(
            event.payload["consult_parent_session_id"].as_str(),
            expected_parent
        );
        assert_eq!(
            event.payload["consult_session_id"].as_str(),
            expected_consult
        );
        assert_eq!(
            event.payload["leg_role"],
            if trusted { "agent" } else { "callee" }
        );
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

#[tokio::test]
async fn external_direct_leg_uses_global_correlation_header_without_trunk_destination() {
    let _serial = lock_settings_serial();
    use crate::call::session_id::{SessionIdSettings, init_settings, settings};
    struct Restore(SessionIdSettings);
    impl Drop for Restore {
        fn drop(&mut self) {
            init_settings(
                self.0.length,
                self.0.header_mode,
                self.0.inherit_callid,
                self.0.source_header.clone(),
            );
        }
    }
    let _restore = Restore(settings());
    let mut session = trunk_correlation_session(false).await;
    session.server.data_context.trunks.clear();
    session.server.data_context.trunks.insert(
        "fromsbc".into(),
        crate::proxy::routing::TrunkConfig::default(),
    );
    let previous = settings();
    init_settings(
        previous.length,
        previous.header_mode,
        previous.inherit_callid,
        Some("X-RustPBX-SessionID".into()),
    );
    for uri in ["sip:1001@localhost", "sip:1001@[::1]"] {
        let internal = crate::call::Location {
            aor: uri.parse().unwrap(),
            ..Default::default()
        };
        let (option, ..) = session
            .build_target_invite_option(&internal, None, None)
            .await
            .unwrap();
        assert_eq!(header_value_of(&option, "X-RustPBX-SessionID"), None);
    }
    for registered in [false, true] {
        let mut target = crate::call::Location {
            aor: "sip:9001@192.0.2.90:5070".parse().unwrap(),
            ..Default::default()
        };
        if registered {
            target.registered_aor = Some("sip:9001@example.test".parse().unwrap());
        }
        let (option, ..) = session
            .build_target_invite_option(&target, None, None)
            .await
            .unwrap();
        assert_eq!(
            header_value_of(&option, "X-RustPBX-SessionID").as_deref(),
            if registered { None } else { Some("ORD-88923") },
            "external direct dials need the root while registration legs stay internal"
        );
    }
}
