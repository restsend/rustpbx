use super::*;

/// Serializes tests that touch the process-global session-id settings:
/// cargo runs `#[tokio::test]`s on parallel worker threads, so the
/// seed/restore pattern alone is racy (a Native-mode assertion can observe
/// the Rfc7989 seed another test installed).
static SETTINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

async fn inherited_session_id_session(header_id: &str, uui_id: Option<&str>) -> SipSession {
    inherited_session_id_session_with_enabled(header_id, uui_id, true).await
}

async fn inherited_session_id_session_with_enabled(
    header_id: &str,
    uui_id: Option<&str>,
    enabled: bool,
) -> SipSession {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::config::ProxyConfig;
    use crate::proxy::tests::common::{
        create_test_request, create_test_server_with_rwi_gateway, create_transaction,
    };

    let gateway = crate::rwi::gateway::RwiGateway::new();
    let (server, _) = create_test_server_with_rwi_gateway(
        ProxyConfig::default(),
        Arc::new(parking_lot::RwLock::new(gateway)),
    )
    .await;
    let mut request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "example.test",
        None,
    );
    request.headers.push(
        rsipstack::sip::headers::SessionId::new(format!(
            "{header_id};remote={}",
            rsipstack::sip::headers::SessionId::NIL
        ))
        .into(),
    );
    if let Some(id) = uui_id {
        request.headers.push(crate::call::uui::build_uui_header(
            id,
            None,
            None,
            None,
            Some("agent"),
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
    let context = CallContext {
        session_id: local_id.into(),
        dialplan: Arc::new(dialplan),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "alice".into(),
        original_callee: "bob".into(),
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

#[tokio::test]
async fn session_id_rfc_padding_preserves_original_root_without_truncating_other_ids() {
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
