use super::*;

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
            session_id_header: Some("X-GSession-ID".to_string()),
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
        header_value_of(&option, "X-GSession-ID").as_deref(),
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
        header_value_of(&option, "X-GSession-ID").as_deref(),
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
                session_id_header: Some("X-GSession-ID".to_string()),
                ..Default::default()
            },
        );
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
        outbound_trunk: Some(OutboundTrunkContext {
            id: Some(1),
            name: "target-g".to_string(),
            dest: None,
            tenant_id: None,
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
        invite.contains(&format!("\r\nx-gsession-id: {root}")),
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
