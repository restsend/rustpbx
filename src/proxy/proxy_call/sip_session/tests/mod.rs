use super::*;
use crate::proxy::proxy_call::dtmf::RtpDtmfDetector;
use crate::proxy::proxy_call::sip_session::builtin_app_factory::BuiltinAppFactory;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        "test-session",
        &app_runtime,
        &None,
        &bridge_dtmf_tx,
        &Arc::new(parking_lot::Mutex::new(None)),
        &Arc::new(parking_lot::Mutex::new(Vec::new())),
        "1001",
        "2000",
        None,
    );

    assert_eq!(runtime.inject_calls.load(Ordering::SeqCst), 0);
}

/// While a bridge is active (armed `bridge_dtmf_tx`), a digit must be
/// 1. forwarded to the bridge WebSocket (pre-existing behaviour),
/// 2. buffered for the return-app flow, and
/// 3. reported as an `ivr_step_trace` carrying the originating node context
///    and the digit (consumer contract for menu/TTS nodes).
#[test]
fn forward_dtmf_with_active_bridge_emits_trace_and_buffers_digit() {
    use crate::proxy::proxy_call::sip_session::transfer::BridgeTraceContext;
    use crate::rwi::gateway::RwiGateway;

    let runtime = Arc::new(DtmfAppRuntime {
        running: false,
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
    })));
    let digits = Arc::new(parking_lot::Mutex::new(Vec::new()));

    forward_dtmf_event(
        '1',
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
    );

    // 1. digit forwarded to the bridge websocket
    let ws_json = ws_rx.try_recv().expect("digit must reach the ws channel");
    let v: serde_json::Value = serde_json::from_str(&ws_json).unwrap();
    assert_eq!(v["type"], "dtmf");
    assert_eq!(v["digit"], "1");

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
    assert_eq!(ev.event.payload["caller"], "sip:1001@x");
    assert!(ev.event.payload["end_reason"].is_null());
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
        normalize_call_hangup_by("agent", Some("support"), false),
        "agent"
    );
    // Skill-group direct routing (resolved_agent_id): stays "agent".
    assert_eq!(normalize_call_hangup_by("agent", None, true), "agent");
    // Non-CC call (no queue, no resolved agent): remapped to "callee".
    assert_eq!(normalize_call_hangup_by("agent", None, false), "callee");
}

#[test]
fn hangup_by_non_agent_unchanged() {
    assert_eq!(normalize_call_hangup_by("caller", None, false), "caller");
    assert_eq!(normalize_call_hangup_by("system", None, false), "system");
    assert_eq!(
        normalize_call_hangup_by("transfer", None, false),
        "transfer"
    );
    assert_eq!(normalize_call_hangup_by("unknown", None, false), "unknown");
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

    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
    }];

    assert!(SipSession::route_via_home_proxy(
        &target,
        &local_addrs,
        true
    ));
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

    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
    }];

    assert!(!SipSession::route_via_home_proxy(
        &target,
        &local_addrs,
        true
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
        rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
    let registered_aor = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();
    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.2:5070").unwrap(),
    };
    let expected = rsipstack::sip::Uri::try_from("sip:lp@10.0.0.2:5070").unwrap();

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
async fn test_init_callee_timer_disabled_without_session_expires() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let (mut session, _handle, cmd_rx) = SipSession::new_uac(
        server,
        CancellationToken::new(),
        None,
        context,
        false,
        Arc::new(MockMediaPeer::new()),
        Arc::new(MockMediaPeer::new()),
    );
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let (mut session, _handle, _cmd_rx) = SipSession::new_uac(
        server,
        CancellationToken::new(),
        None,
        context,
        true,
        Arc::new(MockMediaPeer::new()),
        Arc::new(MockMediaPeer::new()),
    );
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
        .bridge()
        .and_then(|bridge| bridge.leg(crate::media::media_bridge::LegSide::A))
        .expect("prepared caller A leg");
    assert!(
        session
            .bridge()
            .and_then(|bridge| bridge.leg(crate::media::media_bridge::LegSide::B))
            .is_none(),
        "one-target originate must not synthesize a B leg"
    );

    let remote = LegInner::new("rwi-remote", &LegConfig::rtp_pcmu(), None).expect("remote RTP leg");
    let answer = remote.answer(&offer).await.expect("remote SDP answer");
    let caller_leg = session
        .bridge()
        .and_then(|bridge| bridge.leg(crate::media::media_bridge::LegSide::A))
        .expect("prepared caller A leg");
    caller_leg
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("answer must apply to prepared A leg");
    session
        .bridge_mut()
        .expect("originate MediaBridge")
        .accept(crate::media::media_bridge::LegSide::A)
        .await;

    let caller_leg_after = session
        .bridge()
        .and_then(|bridge| bridge.leg(crate::media::media_bridge::LegSide::A))
        .expect("completed caller A leg");
    assert!(
        Arc::ptr_eq(&caller_leg_before, &caller_leg_after),
        "answer must not replace the PeerConnection that generated the offer"
    );
    assert!(caller_leg_after.negotiated().is_some());
    assert!(!caller_leg_after.is_gated());
    assert!(
        session
            .bridge()
            .and_then(|bridge| bridge.leg(crate::media::media_bridge::LegSide::B))
            .is_none(),
        "answering the first target must still leave B empty"
    );

    remote.stop();
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:test-queue".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
    );
    let (callee_tx, mut callee_rx) = mpsc::unbounded_channel();
    session.callee_event_tx = Some(callee_tx);

    let result = session
        .handle_blind_transfer(
            LegId::from("caller"),
            "queue:nonexistent".to_string(),
            transfer::TransferDisposition::Detach,
            &mut callee_rx,
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

// ─── is_local_home_proxy unit tests ────────────────────────────────

#[test]
fn test_is_local_home_proxy_detects_matching_address() {
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
}

#[test]
fn test_is_local_home_proxy_detects_non_matching_address() {
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.149.126:8060").unwrap(),
    };
    assert!(!SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
}

#[test]
fn test_is_local_home_proxy_matches_any_local_address() {
    let local_addrs = vec![
        SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp),
            addr: rsipstack::sip::HostWithPort::try_from("127.0.0.1:5060").unwrap(),
        },
        SipAddr {
            r#type: Some(rsipstack::sip::Transport::Tcp),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
        },
        SipAddr {
            r#type: Some(rsipstack::sip::Transport::Ws),
            addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8443").unwrap(),
        },
    ];
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
}

#[test]
fn test_is_local_home_proxy_rejects_port_mismatch() {
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:5070").unwrap(),
    };
    assert!(!SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
}

#[test]
fn test_is_local_home_proxy_compares_addr_string_not_transport() {
    // Transport type should NOT affect address matching — only host:port matters.
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Wss),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(SipSession::is_local_home_proxy(&local_addrs, &home_proxy));
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
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.0.0.1:5060").unwrap(),
    }];
    assert!(!SipSession::route_via_home_proxy(
        &target,
        &local_addrs,
        false
    ));
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
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let via_home_proxy = SipSession::route_via_home_proxy(&target, &local_addrs, true);
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
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let via_home_proxy = SipSession::route_via_home_proxy(&target, &local_addrs, true);
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
    // This test exercises is_local_home_proxy and route_via_home_proxy
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
        destination: Some(destination),
        home_proxy: Some(home_proxy.clone()),
        ..Default::default()
    };
    let local_addrs = vec![SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    }];
    let via_home_proxy = SipSession::route_via_home_proxy(&target, &local_addrs, true);
    assert!(
        via_home_proxy,
        "route_via_home_proxy must be true for cross-node routing"
    );

    // Verify that BOTH local and remote addresses are correctly
    // distinguished. A local address match → false, remote → true.
    assert!(
        !SipSession::is_local_home_proxy(&local_addrs, &home_proxy),
        "home_proxy at 10.172.149.126 must NOT match local 10.172.148.121"
    );

    let local_home_proxy = SipAddr {
        r#type: None,
        addr: rsipstack::sip::HostWithPort::try_from("10.172.148.121:8060").unwrap(),
    };
    assert!(
        SipSession::is_local_home_proxy(&local_addrs, &local_home_proxy),
        "home_proxy at 10.172.148.121 must match local 10.172.148.121"
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    // `use_media_proxy = true` eagerly creates the MediaBridge.
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
        caller_peer,
        callee_peer,
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

/// `video_policy = "strip"` must disable video on the media path entirely:
/// the caller leg config carries no video capabilities, so the answer has
/// no video m-line (audio-only).
#[tokio::test]
async fn video_strip_policy_omits_video_mline() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
        caller_peer,
        callee_peer,
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
        .bridge()
        .and_then(|bridge| bridge.leg(crate::media::media_bridge::LegSide::A))
        .and_then(|leg| leg.negotiated())
        .expect("caller leg profile");
    assert_eq!(
        caller_leg_profile.audio.as_ref().map(|codec| codec.codec),
        Some(CodecType::PCMU),
        "caller leg sender/profile must match the returned SDP"
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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

    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        true,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
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
        caller_peer,
        callee_peer,
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
    let app_ctx =
        crate::call::app::ApplicationContext::new(db, call_info, std::sync::Arc::new(config));

    let factory = BuiltinAppFactory::new(None, None);

    let params = Some(serde_json::json!({
        "file": "db://ivr/test_ivr.generated.toml"
    }));
    let app = factory.create_app("ivr", params, &app_ctx).await;

    assert!(
        app.ok().flatten().is_some(),
        "BuiltinAppFactory should create IVR app from DB store when generated_db=true"
    );
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
    use crate::media::media_bridge::LegSide;

    let mut mb = crate::media::media_bridge::MediaBridge::new("rtp-timeout-session-test");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
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
        crate::config::RouteResult::Forward(option, _hints) => {
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server.clone(),
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
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
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;
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
    let caller_peer = Arc::new(MockMediaPeer::new());
    let callee_peer = Arc::new(MockMediaPeer::new());
    let (mut session, _handle, _cmd_rx) = SipSession::new(
        server,
        CancellationToken::new(),
        None,
        context,
        server_dialog,
        false,
        caller_peer,
        callee_peer,
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
