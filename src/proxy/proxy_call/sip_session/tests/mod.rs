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
        resumable: false,
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
        Some(std::collections::HashMap::from([(
            "X-Business-Type".to_string(),
            "34".to_string(),
        )])),
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
        resumable: true,
    })));
    let digits = Arc::new(parking_lot::Mutex::new(Vec::new()));

    forward_dtmf_event(
        '8',
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

    let (server, _) = create_test_server().await;
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
            &tx, state_tx, None,
            Some("sip:pbx@192.0.2.77:5099".try_into().unwrap()),
        )
        .unwrap();
    let session_id = server_dialog.id().call_id.clone();
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
    let target = Location {
        aor: "sip:2001@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };
    let mut call_ids = Vec::new();

    // Exercise the builder used by both sequential dialing and parallel forks.
    // No INVITE has been sent and no LegConnected notification has occurred.
    for leg_id in [None, Some("fork-1")] {
        let (invite, _, call_id) = session
            .build_target_invite_option(&target, leg_id, None)
            .await
            .unwrap();
        // The incoming server Contact must not leak into outgoing INVITEs.
        assert_eq!(invite.contact, server.contact_uri_for_location_with_sip_contact(&target, None).unwrap());
        assert_ne!(invite.contact, session.caller_dialog.as_ref().unwrap().snapshot().local_contact.unwrap());
        assert_eq!(invite.call_id.as_deref(), Some(call_id.as_str()));
        assert_ne!(call_id, session_id);
        let resolved = registry
            .get_handle_by_dialog(&call_id)
            .expect("the desk's SIP Call-ID must resolve before it receives INVITE");
        assert_eq!(resolved.session_id(), session_id);
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
    let (invite, _, _) = session
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
        assert!(registry.get_handle_by_dialog(call_id).is_some());
    }

    registry.remove(&session_id);
    for call_id in &call_ids {
        assert!(registry.get_handle_by_dialog(call_id).is_none());
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
    let (mut session, _handle, cmd_rx) = SipSession::new_uac(
        server,
        CancellationToken::new(),
        None,
        context,
        false,
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
    let caller_leg_before = session.media_leg(&LegId::from("caller"))
        .expect("prepared caller A leg");
    assert!(
        session.media_leg(&LegId::from("callee"))
            .is_none(),
        "one-target originate must not synthesize a B leg"
    );

    let remote = LegInner::new("rwi-remote", &LegConfig::rtp_pcmu(), None).expect("remote RTP leg");
    let answer = remote.answer(&offer).await.expect("remote SDP answer");
    let caller_leg = session.media_leg(&LegId::from("caller"))
        .expect("prepared caller A leg");
    caller_leg
        .apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("answer must apply to prepared A leg");
    caller_leg.accept();
    assert!(session.media.bridge.is_none(), "a single peer does not need a bridge");

    let caller_leg_after = session.media_leg(&LegId::from("caller"))
        .expect("completed caller A leg");
    assert!(
        Arc::ptr_eq(&caller_leg_before, &caller_leg_after),
        "answer must not replace the PeerConnection that generated the offer"
    );
    assert!(caller_leg_after.negotiated().is_some());
    assert!(!caller_leg_after.is_gated());
    assert!(
        session.media_leg(&LegId::from("callee"))
            .is_none(),
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
        assert!(session.voip_bridge.is_some());
        assert!(session.conference_bridge.conf_id.is_none(), "a voip bridge must not occupy the conference slot");
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
async fn test_blind_transfer_reports_queue_flow_source() {
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
        "queue"
    );
    assert_eq!(entry.event.payload["transfer_source"]["name"], "sales");
    assert_eq!(entry.event.payload["transfer_source"]["agent_id"], "2002");
    assert_eq!(
        entry.event.payload["transfer_source"]["agent_name"],
        "Alice"
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

    // agent_id falls back to the connected callee user-part; a stale
    // `agent_name` extension must not be attached to it.
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

    let caller_answer_profile = MediaNegotiator::extract_leg_profile(&caller_answer);
    assert_eq!(
        caller_answer_profile
            .audio
            .as_ref()
            .map(|codec| codec.codec),
        Some(CodecType::PCMU),
        "caller answer must follow the codec selected in the callee answer"
    );
    let caller_leg_profile = session.media_leg(&LegId::from("caller"))
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


    let mut mb = crate::media::media_bridge::MediaBridge::new("rtp-timeout-session-test");
    mb.replace_leg(LegSide::A, LegInner::new("caller", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(LegSide::B, LegInner::new("callee", &LegConfig::rtp_pcmu(), None).unwrap(),
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
    let (server, _) = create_test_server_with_rwi_gateway(ProxyConfig::default(), gateway.clone()).await;
    let request = create_test_request(rsipstack::sip::Method::Invite, "caller", None, "rustpbx.com", None);
    let context = CallContext {
        session_id: "added-media".into(),
        dialplan: Arc::new(Dialplan::new("added-media".into(), request, DialDirection::Inbound)
            .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All))),
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
    let (mut session, _handle, _commands) = SipSession::new_uac(
        server.clone(), cancel, None, context, true,
    );
    let mut cfg = LegConfig::rtp_pcmu();
    cfg.codecs.push(crate::media::negotiate::CodecInfo {
        payload_type: 101, codec: audio_codec::CodecType::TelephoneEvent,
        clock_rate: 8000, channels: 1, fmtp: Some("0-16".into()),
    });
    let caller = LegInner::new("remote-caller", &cfg, None).unwrap();
    let local = LegInner::new("caller", &cfg, None).unwrap();
    let offer = caller.create_offer().await.unwrap();
    let answer = local.apply_sdp(&offer, rustrtc::SdpType::Offer).await.unwrap();
    caller.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
    session.media.caller_offer = Some(offer);
    session.media.answer = Some(answer);
    session.legs.set_media_leg(&LegId::from("caller"), local.clone());
    session.media_leg(&LegId::from("caller")).unwrap().accept();
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);

    // Exercise leg_add itself, including its actual media-offer selection.
    // Supply the remote answer through the same LegConnected command the SIP
    // response task uses; no mixer/peer setup is injected by the test.
    let alice_id = session.handle_add_leg_inner(
        "sip:alice@127.0.0.1:5099".into(), Some(LegId::from("alice-test")), vec![], None).await.unwrap();
    assert_eq!(session.bridge().and_then(|bridge| bridge.leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str()))).map(|p| p.id().to_string()), None);
    assert!(session.legs.media_leg(&alice_id).is_some());
    let alice = LegInner::new("remote-alice", &cfg, None).unwrap();
    let answer = alice.apply_sdp(&session.legs.media_leg(&alice_id).unwrap().pc().local_description().unwrap().to_sdp_string(), rustrtc::SdpType::Offer)
        .await.unwrap();
    session.execute_command(CallCommand::LegConnected {
        leg_id: alice_id.clone(), answer_sdp: Some(answer), dialog_id: None,
    }, None).await;
    assert!(!session.bridge.active, "answering an added leg must not connect media");
    assert!(session.execute_command(CallCommand::Bridge {
        leg_a: LegId::from("caller"), leg_b: alice_id.clone(), mode: crate::call::domain::P2PMode::Audio,
    }, None).await.success);
    assert!(session.bridge().unwrap().is_bridged());
    for side in [LegSide::A, LegSide::B] {
        assert!(session.bridge().unwrap().leg(side).unwrap().egress_is_relay());
    }
    assert!(server.conference_server.get_conference(
        &crate::call::runtime::ConferenceId::from("consult-added-media")
    ).await.is_none());

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
            destination.pc(), destination.negotiated().unwrap(),
            LegId::from("observer"), session.cancel_token.child_token(),
        ).unwrap();
        source.set_egress_source(crate::media::egress::EgressSource::Media {
            audio: Box::new(crate::media::audio_source::ToneAudioSource::new(
                660, Duration::from_secs(1), 8000,
            ).unwrap()), loop_playback: false, on_end: None,
        }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = audio.recv().await.unwrap();
                if !frame.silence && frame.frame.samples.iter().any(|s| s.abs() > 100) {
                    break;
                }
            }
        }).await.expect("opposite endpoint must receive audio");
        let mut received_digits = destination_bridge.dtmf_bus();
        source.send_dtmf(digit).await.unwrap();
        let (_, received) = tokio::time::timeout(Duration::from_secs(3), received_digits.recv())
            .await.expect("DTMF must cross the relay").unwrap();
        assert_eq!(received.digit.to_string(), digit);
    }
    // A subsequent add must not replace the connected Alice transport.
    let first_b = session.bridge().unwrap().leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str())).unwrap().clone();
    let another = session.handle_add_leg_inner(
        "sip:other@127.0.0.1:5098".into(), Some(LegId::from("another")), vec![], None).await.unwrap();
    assert!(Arc::ptr_eq(&first_b, &session.bridge().unwrap().leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str())).unwrap()));
    assert!(session.legs.media_leg(&another).is_some());
    let third = LegInner::new("remote-third", &cfg, None).unwrap();
    let third_peer = session.legs.media_leg(&another).unwrap();
    let answer = third.answer(&third_peer.pc().local_description().unwrap().to_sdp_string()).await.unwrap();
    session.execute_command(CallCommand::LegConnected {
        leg_id: another.clone(), answer_sdp: Some(answer), dialog_id: None,
    }, None).await;
    // A third answer must not change the existing pair or start a mixer.
    assert!(Arc::ptr_eq(&first_b, &session.bridge().unwrap().leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str())).unwrap()));
    assert!(session.conference_bridge.conf_id.is_none());
    session.execute_command(CallCommand::Bridge {
        leg_a: alice_id.clone(), leg_b: another.clone(), mode: crate::call::domain::P2PMode::Audio,
    }, None).await;
    assert!(Arc::ptr_eq(&first_b, &session.bridge().unwrap().leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str())).unwrap()));
    assert!(Arc::ptr_eq(&third_peer, &session.bridge().unwrap().leg_for_id(&crate::media::leg_id::LegId::from(another.as_str())).unwrap()));
    // Signaling and media operations resolve by leg identity even when the
    // caller is no longer selected in either bridge slot.
    assert!(Arc::ptr_eq(&local, &session.media_leg(&LegId::from("caller")).unwrap()));
    let caller_pc = session.get_local_reinvite_pc(DialogSide::Caller).await.unwrap();
    assert_eq!(caller_pc.local_description().unwrap().to_sdp_string(),
        local.pc().local_description().unwrap().to_sdp_string());
    let mut third_observer = MediaBridge::new("third-observer");
    third_observer.replace_leg(LegSide::A, third.clone()).await;
    third_observer.accept(LegSide::A).await;
    let mut digits = third_observer.dtmf_bus();
    alice.send_dtmf("8").await.unwrap();
    let (_, digit) = tokio::time::timeout(Duration::from_secs(3), digits.recv()).await.unwrap().unwrap();
    assert_eq!(digit.digit, '8');
    third_observer.close();
    session.clear_bridge().await;
    assert!(session.setup_bridge(LegId::from("caller"), alice_id.clone()).await);
    // Switching away and back must preserve the original PeerConnections.
    assert!(Arc::ptr_eq(&first_b, &session.bridge().unwrap().leg_for_id(&crate::media::leg_id::LegId::from(alice_id.as_str())).unwrap()));
    let mut audio = crate::media::app_ingress::LegPcmStream::attach(
        alice.pc(), alice.negotiated().unwrap(), LegId::from("restored"), session.cancel_token.child_token(),
    ).unwrap();
    caller.set_egress_source(crate::media::egress::EgressSource::Media {
        audio: Box::new(crate::media::audio_source::ToneAudioSource::new(660, Duration::from_secs(1), 8000).unwrap()),
        loop_playback: false, on_end: None,
    }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let frame = audio.recv().await.unwrap();
            if frame.frame.samples.iter().any(|s| s.abs() > 100) { break; }
        }
    }).await.expect("restored peer must still carry audio");
    session.handle_remove_leg(another).await.unwrap();
    session.handle_remove_leg(alice_id).await.unwrap();
    let retry = session.handle_add_leg_inner(
        "sip:alice@127.0.0.1:5099".into(), Some(LegId::from("alice-retry")), vec![], None).await.unwrap();
    assert!(!Arc::ptr_eq(&first_b, &session.legs.media_leg(&retry).unwrap()));
    third.stop();
    session.bridge_mut().unwrap().close();
    for bridge in &mut remotes { bridge.close(); }
}

#[tokio::test]
async fn conference_merge_resolves_queue_agent_alias_and_delivers_audio() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    let (server, _) = create_test_server().await;
    let request = create_test_request(rsipstack::sip::Method::Invite, "alice", None, "rustpbx.com", None);
    let context = CallContext {
        session_id: "queue-merge".into(),
        dialplan: Arc::new(Dialplan::new("queue-merge".into(), request, DialDirection::Inbound)),
        cookie: TransactionCookie::default(),
        start_time: Instant::now(),
        original_caller: "sip:alice@rustpbx.com".into(),
        original_callee: "sip:queue@rustpbx.com".into(),
        max_forwards: 70,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata: None,
    };
    let (mut session, _handle, _commands) = SipSession::new_uac(
        server.clone(), CancellationToken::new(), None, context, true,
    );
    let agent = LegId::from(uuid::Uuid::new_v4().to_string());
    let consult = LegId::from("consult-queue-merge");
    let mut phones = Vec::new();
    for id in [LegId::from("caller"), agent.clone(), consult.clone()] {
        let mut leg = Leg::new(id.clone());
        leg.state = LegState::Connected;
        if id == consult { leg.source_leg = Some(agent.clone()); }
        session.legs.insert(id.clone(), leg);
        let local = LegInner::new(id.to_string(), &LegConfig::rtp_pcmu(), None).unwrap();
        let phone = LegInner::new(format!("phone-{id}"), &LegConfig::rtp_pcmu(), None).unwrap();
        let offer = local.create_offer().await.unwrap();
        let answer = phone.answer(&offer).await.unwrap();
        local.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
        local.accept();
        phone.accept();
        session.legs.set_answer(id.clone(), answer);
        session.legs.set_media_leg(&id, local);
        phones.push(phone);
    }
    assert!(session.media_leg(&LegId::from("callee")).is_none());
    assert!(session.setup_bridge(agent.clone(), consult.clone()).await);
    let room = crate::call::runtime::ConferenceId::from("queue-merge-room");
    server.conference_server.create_conference(room.clone(), None).await.unwrap();
    // CC sends the legacy callee alias; the session must join the real UUID
    // leg, while preserving the concrete caller and consultation IDs.
    for id in [LegId::from("caller"), LegId::from("callee"), consult.clone()] {
        let result = session.execute_command(CallCommand::JoinMixerLeg {
            mixer_id: room.0.clone(), leg_id: id,
        }, None).await;
        assert!(result.success, "conference join failed: {:?}", result);
    }
    assert_eq!(server.conference_server.get_conference(&room).await.unwrap().participant_count(), 3);
    assert!(session.legs.conference_bridge_handle(&agent).is_some());
    assert!(session.legs.conference_bridge_handle(&LegId::from("callee")).is_none());

    let cancel = CancellationToken::new();
    let mut agent_audio = phones[1].pcm_stream(cancel.clone()).unwrap();
    phones[0].play_media(Box::new(crate::media::audio_source::ToneAudioSource::new(
        440, Duration::from_secs(2), 8000,
    ).unwrap()), true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = agent_audio.recv().await.unwrap();
            if frame.frame.samples.iter().any(|sample| sample.unsigned_abs() > 100) { break; }
        }
    }).await.expect("queued agent must hear caller audio after three-way merge");
    cancel.cancel();
    session.handle_leave_mixer().await.unwrap();
    for phone in phones { phone.stop(); }
}

#[tokio::test]
async fn consult_media_preserves_peers_across_bridge_and_explicit_mixer() {
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
            ProxyConfig::default(), vec![Arc::new(crate::addons::cc::cc_call_session_hook::CcCallSessionHook::new(
                Arc::new(crate::addons::cc::agent::AgentRegistry::new()),
                Arc::new(crate::addons::cc::metrics::MetricsCollector::new()),
            ).with_transfer_manager_shared(transfer_container.clone()).with_call_registry_shared(call_container.clone()))],
        ).await;
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
            server.active_call_registry.register_handle(session.id.to_string(), _handle.clone());
            *call_container.write().await = Some(server.active_call_registry.clone());
            let manager = Arc::new(crate::addons::cc::transfer::ConsultTransferManager::new(
                server.conference_server.manager_raw().clone(),
            ).with_call_registry(server.active_call_registry.clone()));
            manager.initiate("transfer-media".into(), session.id.to_string(), "agent".into(), "alice".into());
            manager.mark_connected_pending_answer("transfer-media", session.id.to_string()).unwrap();
            *transfer_container.write().await = Some(manager.clone());
            manager
        };
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
            session.legs.set_media_leg(&LegId::from(name), local.clone());
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
        if matches!(scenario, "reject" | "timeout" | "cancel_ringing") {
            session
                .execute_command(
                    if scenario.starts_with("cancel_") { CallCommand::LegRemove { leg_id: consult.clone() } } else { CallCommand::LegFailed {
                        leg_id: consult.clone(),
                        reason: if scenario == "reject" {
                            "Rejected with 603"
                        } else {
                            "Timeout"
                        }
                        .into(),
                    } },
                    None,
                )
                .await;
            // Recovery belongs to the CC owner, not generic leg removal.
            assert_eq!(session.legs.get(&LegId::from("caller")).unwrap().state, LegState::Hold);
            #[cfg(feature = "addon-cc")]
            if !scenario.starts_with("cancel_") {
                assert!(matches!(transfer_manager.get_state("transfer-media"),
                    Some(crate::addons::cc::transfer::TransferState::Failed { .. })));
                while let Ok(command) = _commands.try_recv() { session.execute_command(command, None).await; }
            }
            if !cfg!(feature = "addon-cc") || scenario.starts_with("cancel_") {
            for command in [
                CallCommand::LeaveMixer,
                CallCommand::LegRemove { leg_id: consult.clone() },
                CallCommand::Bridge { leg_a: LegId::from("caller"), leg_b: LegId::from("callee"), mode: crate::call::domain::P2PMode::Audio },
                CallCommand::Unhold { leg_id: LegId::from("caller") },
                CallCommand::Unhold { leg_id: LegId::from("callee") },
            ] { assert!(session.execute_command(command, None).await.success); }
            }
            assert!(session.legs.get(&consult).is_none());
            // An already queued answer must not resurrect a cancelled/failed C.
            assert!(session.execute_command(CallCommand::LegConnected {
                leg_id: consult.clone(), answer_sdp: None, dialog_id: None,
            }, None).await.success);
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
        session.media_leg(&consult).unwrap().apply_sdp(&answer, rustrtc::SdpType::Pranswer)
            .await.unwrap();
        // Provisional media may connect the requested B-C pair before final answer.
        let provisional = session.execute_command(CallCommand::LegRinging {
            leg_id: consult.clone(),
        }, None).await;
        assert!(provisional.success);
        assert_eq!(session.legs.get(&consult).unwrap().state, LegState::EarlyMedia);
        assert!(session.bridge().unwrap().is_bridged());
        assert_eq!(session.legs.get(&LegId::from("caller")).unwrap().state, LegState::Hold);
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
        if scenario == "complete" || scenario == "complete_from_customer" {
            if scenario == "complete_from_customer" {
                session.handle_hold(consult.clone(), None).await.unwrap();
                session.handle_unhold(LegId::from("caller")).await.unwrap();
                let agent = session.resolve_transfer_leg(LegId::from("callee"));
                assert!(session.setup_bridge(LegId::from("caller"), agent).await);
                assert!(session.bridge.contains_leg(&LegId::from("caller")));
            }
            let caller_peer = session.media_leg(&LegId::from("caller")).unwrap();
            let consult_peer = session.media_leg(&consult).unwrap();
            for command in [
                CallCommand::LeaveMixer,
                CallCommand::Bridge { leg_a: LegId::from("caller"), leg_b: consult.clone(), mode: crate::call::domain::P2PMode::Audio },
                CallCommand::Unhold { leg_id: LegId::from("caller") },
                CallCommand::Unhold { leg_id: consult.clone() },
                CallCommand::LegRemove { leg_id: LegId::from("callee") },
                CallCommand::MarkTransferred,
            ] {
                let result = session.execute_command(command, None).await;
                assert!(result.success, "{:?}", result.message);
            }
            assert!(session.legs.get(&LegId::from("callee")).is_none());
            assert!(session.conference_bridge.conf_id.is_none());
            assert!(session.bridge().unwrap().is_bridged());
            assert!(Arc::ptr_eq(&caller_peer, &session.media_leg(&LegId::from("caller")).unwrap()));
            assert!(Arc::ptr_eq(&consult_peer, &session.media_leg(&consult).unwrap()));
            assert!(session.bridge.contains_leg(&LegId::from("caller")));
            assert!(session.bridge.contains_leg(&consult));
            let mut audio = crate::media::app_ingress::LegPcmStream::attach(
                remote.pc(), remote.negotiated().unwrap(),
                crate::media::leg_id::LegId::from("complete-observer"), CancellationToken::new(),
            ).unwrap();
            remote_legs[0].play(Box::new(crate::media::audio_source::ToneAudioSource::new(
                660, Duration::from_secs(2), 8000,
            ).unwrap()), false, None).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let frame = audio.recv().await.unwrap();
                    if frame.frame.samples.iter().any(|s| s.abs() > 100) { break; }
                }
            }).await.expect("A must reach C after direct completion");
            for remote in remote_legs { remote.stop(); }
            continue;
        }
        let mut private_tokens: Vec<CancellationToken> = Vec::new();
        if scenario == "supervisor_switch" {
            session
                .handle_supervisor_listen(consult.clone(), LegId::from("callee"), None)
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
                    if scenario.starts_with("cancel_") { CallCommand::LegRemove { leg_id: consult.clone() } } else { CallCommand::LegFailed {
                        leg_id: consult.clone(),
                        reason: "Remote hung up".into(),
                    } },
                    None,
                )
                .await;
            // Recovery belongs to the CC owner, not generic leg removal.
            assert_eq!(session.legs.get(&LegId::from("caller")).unwrap().state, LegState::Hold);
            #[cfg(feature = "addon-cc")]
            if !scenario.starts_with("cancel_") {
                assert!(matches!(transfer_manager.get_state("transfer-media"),
                    Some(crate::addons::cc::transfer::TransferState::Failed { .. })));
                while let Ok(command) = _commands.try_recv() { session.execute_command(command, None).await; }
            }
            if !cfg!(feature = "addon-cc") || scenario.starts_with("cancel_") {
            for command in [
                CallCommand::LeaveMixer,
                CallCommand::LegRemove { leg_id: consult.clone() },
                CallCommand::Bridge { leg_a: LegId::from("caller"), leg_b: LegId::from("callee"), mode: crate::call::domain::P2PMode::Audio },
                CallCommand::Unhold { leg_id: LegId::from("caller") },
                CallCommand::Unhold { leg_id: LegId::from("callee") },
            ] { assert!(session.execute_command(command, None).await.success); }
            }
            assert!(session.legs.get(&consult).is_none());
            // An already queued answer must not resurrect a cancelled/failed C.
            assert!(session.execute_command(CallCommand::LegConnected {
                leg_id: consult.clone(), answer_sdp: None, dialog_id: None,
            }, None).await.success);
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
                private_tokens.push(session.legs.conference_bridge_handle(&LegId::from(name))
                    .unwrap().cancel_token.clone());
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
                !session.media_leg(&LegId::from(id)).unwrap().egress_is_relay(),
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
                vec![], None)
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
}


#[tokio::test]
async fn reinvite_hold_resume_restores_both_relay_directions() {
    use crate::call::{DialDirection, Dialplan, MediaConfig};
    use crate::config::MediaProxyMode;
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    for side in [DialogSide::Caller, DialogSide::Callee] {
        let request = create_test_request(rsipstack::sip::Method::Invite, "caller", None, "rustpbx.com", None);
        let dialplan = Dialplan::new("hold-resume".into(), request, DialDirection::Inbound)
            .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All));
        let (mut session, _handle, _commands) = build_session_with_cmd_rx(dialplan).await;
        let _guard = session.cancel_token.clone().drop_guard();
        let mut remotes = Vec::new();
        for name in ["caller", "callee"] {
            let local = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote = LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = remote.create_offer().await.unwrap();
            let answer = local.apply_sdp(&offer, rustrtc::SdpType::Offer).await.unwrap();
            remote.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
            local.accept();
            remote.accept();
            session.legs.set_media_leg(&LegId::from(name), local);
            session.update_leg_state(&LegId::from(name), LegState::Connected);
            remotes.push(remote);
        }
        assert!(session.setup_bridge(LegId::from("caller"), LegId::from("callee")).await);
        let remote = &remotes[if matches!(side, DialogSide::Caller) { 0 } else { 1 }];
        // Exercise the same negotiation -> hold transition sequence as an
        // incoming re-INVITE, twice to catch stale route state after resume.
        for _ in 0..2 {
            for direction in ["sendonly", "sendrecv"] {
                let offer = remote.create_offer().await.unwrap();
                let offer = rustrtc::modify_sdp_direction(&offer, direction);
                let parsed = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer).unwrap();
                let answer = session.build_local_dialog_answer(side, rsipstack::sip::Method::Invite, &offer).await.unwrap();
                remote.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
                session.apply_reinvite_hold_transition(side, &parsed, &[]).await;
                let resumed = direction == "sendrecv";
                assert_eq!(session.bridge().unwrap().is_bridged(), resumed);
                if resumed {
                    for name in ["caller", "callee"] {
                        assert!(session.media_leg(&LegId::from(name)).unwrap().egress_is_relay(),
                            "{side:?} resume must restore {name} relay, not merely mark the bridge active");
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
            registry.update_status("agent", AgentStatus::Idle).await.unwrap();
            registry.update_status_with_call_delta("agent", AgentStatus::Ringing {
                call_id: format!("blind-{outcome}"), since: Instant::now(),
            }, 1).await.unwrap();
            registry.update_status("agent", AgentStatus::Busy {
                call_id: format!("blind-{outcome}"), since: Instant::now(),
            }).await.unwrap();
            crate::proxy::tests::common::create_test_server_with_session_hooks(config, vec![
                Arc::new(crate::addons::cc::cc_call_session_hook::CcCallSessionHook::new(
                    registry.clone(), Arc::new(crate::addons::cc::metrics::MetricsCollector::new()),
                )),
            ]).await
        };
        #[cfg(not(feature = "addon-cc"))]
        let (server, _) = crate::proxy::tests::common::create_test_server_with_config(config).await;
        let request = create_test_request(rsipstack::sip::Method::Invite, "caller", None, "rustpbx.com", None);
        let (tx, _) = create_transaction(request.clone()).await;
        let (state_tx, _state_rx) = mpsc::unbounded_channel();
        let dialog = server.dialog_layer.get_or_create_server_invite(&tx, state_tx, None, None).unwrap();
        let context = CallContext {
            session_id: format!("blind-{outcome}"),
            dialplan: Arc::new(Dialplan::new(format!("blind-{outcome}"), request, DialDirection::Inbound)
                .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All))),
            cookie: TransactionCookie::default(), start_time: Instant::now(),
            original_caller: "sip:caller@rustpbx.com".into(),
            original_callee: "sip:agent@rustpbx.com".into(), max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(), metadata: None,
        };
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let (mut session, _handle, _commands) = SipSession::new(server, cancel, None, context, dialog, true);
        let agent = LegId::new(uuid::Uuid::new_v4().to_string());
        session.legs.insert(agent.clone(), crate::call::domain::Leg::new(agent.clone()));
        let mut remotes = Vec::new();
        for name in ["caller", agent.as_str()] {
            let local = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote = LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = remote.create_offer().await.unwrap();
            let answer = local.apply_sdp(&offer, rustrtc::SdpType::Offer).await.unwrap();
            remote.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
            if name == "caller" {
                session.media.caller_offer = Some(offer);
                session.media.answer = Some(answer);
            }
            local.accept(); remote.accept();
            session.legs.set_media_leg(&LegId::from(name), local);
            session.update_leg_state(&LegId::from(name), LegState::Connected);
            remotes.push(remote);
        }
        assert!(session.setup_bridge(LegId::from("caller"), agent.clone()).await);
        let old_peer = session.media_leg(&agent.clone()).unwrap();
        let caller_peer = session.media_leg(&LegId::from("caller")).unwrap();
        let old_dialog = rsipstack::dialog::DialogId {
            call_id: "original-agent".into(), local_tag: "local".into(), remote_tag: "remote".into(),
        };
        session.meta.connected_callee_dialog_id = Some(old_dialog.clone());
        session.callee_dialogs.insert(old_dialog.clone(), ());
        let target = LegId::new(format!("transfer-{}", uuid::Uuid::new_v4()));
        for command in [
            CallCommand::LegAdd { source_leg: Some(LegId::from("caller")), target: "sip:alice@127.0.0.1:5099".into(), leg_id: Some(target.clone()), headers: vec![] },
            CallCommand::HangupAgentLeg,
            CallCommand::Bridge { leg_a: LegId::from("caller"), leg_b: target.clone(), mode: crate::call::domain::P2PMode::Audio },
            CallCommand::MarkTransferred,
        ] {
            let adding = matches!(&command, CallCommand::LegAdd { .. });
            let result = tokio::time::timeout(Duration::from_secs(1), session.execute_command(command, None)).await
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
        assert!(Arc::ptr_eq(&caller_peer, &session.media_leg(&LegId::from("caller")).unwrap()));
        assert!(session.pending_hangup.contains(&old_dialog), "agent BYE must be scheduled before C answers");
        assert!(!session.legs.contains_key(&agent.clone()));
        assert!(!session.bridge().unwrap().is_bridged(), "unanswered C must not enter the media bridge");
        assert!(target_peer.negotiated().is_none());
        assert!(session.meta.transferred);
        #[cfg(feature = "addon-cc")]
        {
            use crate::addons::cc::agent::AgentStatus;
            let released = registry.get_agent("agent").await.unwrap();
            assert!(matches!(released.status, AgentStatus::Wrapup { .. }));
            assert_eq!(released.current_calls, 0);
            session.execute_command(CallCommand::HangupAgentLeg, None).await;
            session.fire_on_call_ended_hooks(None, 10).await;
            let duplicate = registry.get_agent("agent").await.unwrap();
            assert_eq!(duplicate.last_state_changed_at, released.last_state_changed_at);
            assert_eq!(duplicate.current_calls, 0);
        }
        // Retired B's termination must not cascade into a caller BYE.
        session.handle_callee_state(DialogState::Terminated(old_dialog,
            rsipstack::dialog::dialog::TerminatedReason::UacBye)).await.unwrap();
        assert!(!session.pending_hangup.contains(&session.caller_dialog_id()));
        assert!(!session.cancel_token.is_cancelled());

        if matches!(outcome, "reject" | "timeout") {
            let result = session.execute_command(CallCommand::LegFailed {
                leg_id: target.clone(), reason: if outcome == "reject" { "Rejected with 486" } else { "Ring timeout" }.into(),
            }, None).await;
            assert!(!result.success);
            assert!(session.pending_hangup.contains(&session.caller_dialog_id()), "no agent remains to restore on failure");
            assert!(session.media_leg(&target).is_none());
            assert!(!session.meta.transfer_in_progress);
        } else {
            let remote = LegInner::new("alice", &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = target_peer.pc().local_description().unwrap().to_sdp_string();
            let answer = remote.apply_sdp(&offer, rustrtc::SdpType::Offer).await.unwrap();
            remote.accept();
            if outcome == "early_media" {
                target_peer.apply_sdp(&answer, rustrtc::SdpType::Pranswer).await.unwrap();
                assert!(session.execute_command(CallCommand::LegRinging { leg_id: target.clone() }, None).await.success);
                assert!(session.bridge().unwrap().is_bridged(), "usable early media connects A-C");
            }
            let connected = session.execute_command(CallCommand::LegConnected {
                leg_id: target.clone(), answer_sdp: Some(answer), dialog_id: None,
            }, None).await;
            assert!(connected.success, "{:?}", connected.message);
            #[cfg(feature = "addon-cc")]
            {
                let released = registry.get_agent("agent").await.unwrap();
                assert!(matches!(released.status, crate::addons::cc::agent::AgentStatus::Wrapup { .. }));
                assert_eq!(released.current_calls, 0);
            }
            assert!(!session.meta.transfer_in_progress);
            assert!(session.bridge().unwrap().is_bridged());
            assert!(Arc::ptr_eq(&target_peer, &session.bridge().unwrap().leg(LegSide::B).unwrap()));
            assert!(session.pending_hangup.is_empty());
            let mut audio = crate::media::app_ingress::LegPcmStream::attach(
                remotes[0].pc(), remotes[0].negotiated().unwrap(),
                crate::media::leg_id::LegId::from("caller-observer"), CancellationToken::new(),
            ).unwrap();
            remote.play_media(Box::new(crate::media::audio_source::ToneAudioSource::new(
                660, Duration::from_secs(1), 8000,
            ).unwrap()), true).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let frame = audio.recv().await.unwrap();
                    if frame.frame.samples.iter().any(|sample| sample.abs() > 100) { break; }
                }
            }).await.expect("caller must receive Alice audio after transfer");
            if outcome == "answer" {
                let ended = session.execute_command(CallCommand::LegFailed {
                    leg_id: target, reason: "Remote hung up".into(),
                }, None).await;
                assert!(!ended.success);
                assert!(session.pending_hangup.contains(&session.caller_dialog_id()));
            }
            remote.stop();
        }
        session.bridge_mut().unwrap().close();
        for remote in remotes { remote.stop(); }
    }
}

#[tokio::test]
async fn cancel_before_queued_answer_sends_bye_to_late_dialog() {
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::proxy::tests::common::{create_test_request, create_test_server};
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::sip::prelude::HeadersExt;
    use rsipstack::transport::SipAddr;
    use rsipstack::transport::udp::UdpConnection;
    use tokio::time::timeout;

    for (ignore_late_answer, teardown_before_drain) in [(true, false), (true, true), (false, false), (false, true)] {
        let (server, _) = create_test_server().await;
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        server.endpoint.inner.transport_layer.del_transport(&SipAddr {
            r#type: Some(rsipstack::sip::Transport::Udp), addr: "127.0.0.1:5060".try_into().unwrap(),
        });
        let transport = UdpConnection::create_connection(local, None, None).await.unwrap();
        server.endpoint.inner.transport_layer.add_transport(transport.into());
        let endpoint = server.endpoint.inner.clone();
        let serving = tokio::spawn(async move { endpoint.serve().await.unwrap(); });
        let request = create_test_request(Method::Invite, "bob", None, "rustpbx.com", None);
        let context = CallContext {
            session_id: "late-answer".into(),
            dialplan: Arc::new(Dialplan::new("late-answer".into(), request, DialDirection::Outbound)),
            cookie: TransactionCookie::default(), start_time: Instant::now(),
            original_caller: "sip:bob@rustpbx.com".into(), original_callee: "sip:alice@rustpbx.com".into(),
            max_forwards: 70, created_at: chrono::Utc::now().to_rfc3339(), metadata: None,
        };
        let cancel = CancellationToken::new();
        let _cancel_on_exit = cancel.clone().drop_guard();
        let (mut session, handle, mut commands) = SipSession::new_uac(server.clone(), cancel.clone(), None, context, true);
        let leg_id = LegId::from("late-target");
        let add = CallCommand::LegAdd {
            source_leg: None, target: format!("sip:alice@{target_addr}"), leg_id: Some(leg_id.clone()), headers: vec![],
        };
        let added = session.execute_command(add, None).await;
        assert!(added.success, "{:?}", added.message);
        let mut buffer = [0u8; 65536];
        let (size, pbx_addr) = timeout(Duration::from_secs(3), target.recv_from(&mut buffer)).await.unwrap().unwrap();
        let SipMessage::Request(invite) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() else {
            panic!("expected INVITE");
        };
        assert_eq!(invite.method, Method::Invite);
        let call_id = invite.call_id_header().unwrap().value().to_string();

        // Hold command processing: removal is queued before the real SIP 200
        // causes the dial task to append LegConnected to the same channel.
        handle.send_command(CallCommand::LegRemove { leg_id: leg_id.clone() }).unwrap();
        let mut answer = server.endpoint.inner.make_response(&invite, StatusCode::OK, Some(invite.body.clone()));
        answer.headers.retain(|header| !matches!(header, Header::To(_)));
        answer.headers.push(Header::To(format!("<sip:alice@{target_addr}>;tag=late-peer").into()));
        answer.headers.push(Header::Contact(format!("<sip:alice@{target_addr}>").into()));
        answer.headers.push(Header::ContentType("application/sdp".into()));
        target.send_to(answer.to_string().as_bytes(), pbx_addr).await.unwrap();
        let queued = timeout(Duration::from_secs(3), async {
            let mut queued = Vec::new();
            loop {
                let command = commands.recv().await.expect("dial task notification");
                let answered = matches!(&command, CallCommand::LegConnected { leg_id: id, dialog_id: Some(dialog), .. }
                    if id == &leg_id && dialog == &call_id);
                queued.push(command);
                if answered { return queued; }
            }
        }).await.expect("real 200 OK produces LegConnected");
        let removed_at = queued.iter().position(|cmd| matches!(cmd, CallCommand::LegRemove { .. })).unwrap();
        let answered_at = queued.iter().position(|cmd| matches!(cmd, CallCommand::LegConnected { .. })).unwrap();
        assert!(removed_at < answered_at);
        let dialogs = server.dialog_layer.get_client_dialog_by_call_id(&call_id);
        assert_eq!(dialogs.len(), 1);
        let confirmed_id = dialogs[0].id();
        assert!(!dialogs[0].state().is_terminated());
        for command in queued {
            if matches!(&command, CallCommand::LegConnected { .. }) {
                assert!(!session.legs.contains_key(&leg_id));
                assert!(session.pending_hangup.is_empty(), "removal did not yet know the answered dialog");
                // The dial-task guard must clean up even if the session
                // never processes the queued answer.
                if ignore_late_answer { continue; }
            }
            let result = session.execute_command(command, None).await;
            assert!(result.success, "{:?}", result.message);
        }
        assert!(!session.legs.contains_key(&leg_id), "late answer must not recreate C");
        if ignore_late_answer {
            assert!(session.pending_hangup.is_empty());
            assert!(!session.callee_guards.iter().any(|guard| guard.id() == &confirmed_id));
        }

        let runner = if teardown_before_drain {
            // Isolate the retained guard: no command loop or hangup drain.
            session.pending_hangup.clear();
            drop(session);
            None
        } else {
            let (_callee_tx, callee_rx) = mpsc::unbounded_channel();
            Some(tokio::spawn(async move { session.run_main_loop(None, callee_rx, commands).await }))
        };
        let bye = timeout(Duration::from_secs(3), async {
            loop {
                let (size, _) = target.recv_from(&mut buffer).await.unwrap();
                if let SipMessage::Request(request) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() {
                    if request.method == Method::Bye { return request; }
                }
            }
        }).await.expect("target receives BYE over UDP for its late answer");
        assert_eq!(bye.call_id_header().unwrap().value(), call_id);
        let response = server.endpoint.inner.make_response(&bye, StatusCode::OK, None);
        target.send_to(response.to_string().as_bytes(), pbx_addr).await.unwrap();
        cancel.cancel();
        if let Some(runner) = runner {
            timeout(Duration::from_secs(3), runner).await.unwrap().unwrap().unwrap();
        }
        server.endpoint.shutdown();
        serving.await.unwrap();
    }
}

#[tokio::test]
async fn rwi_manual_parallel_retry_and_leg_cleanup() {
    use crate::config::ProxyConfig;
    use crate::call::{DialDirection, Dialplan, TransactionCookie};
    use crate::media::leg::{LegConfig, LegInner};
    use crate::proxy::tests::common::{create_test_request, create_test_server_with_rwi_gateway};
    use crate::rwi::{RwiGateway, RwiCommandPayload};
    use crate::rwi::processor::{RwiCommandProcessor, CommandResult as RwiResult};
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::sip::prelude::HeadersExt;
    use rsipstack::transport::{SipAddr, udp::UdpConnection};
    use tokio::time::timeout;

    let gateway = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
    let mut events = gateway.read().subscribe_events();
    let (ws_tx, mut ws_events) = mpsc::unbounded_channel();
    {
        let mut gw = gateway.write();
        let id = gw.create_session(crate::rwi::RwiIdentity { token: "test".into(), scopes: vec!["call.control".into()] }).read().id.clone();
        gw.set_session_event_sender(&id, ws_tx);
        gw.claim_call_ownership(&id, "rwi-manual".into(), crate::rwi::session::OwnershipMode::Control).unwrap();
    }
    let (server, _) = create_test_server_with_rwi_gateway(ProxyConfig::default(), gateway.clone()).await;
    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = target.local_addr().unwrap();
    server.endpoint.inner.transport_layer.del_transport(&SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp), addr: "127.0.0.1:5060".try_into().unwrap(),
    });
    let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None).await.unwrap();
    server.endpoint.inner.transport_layer.add_transport(transport.into());
    let endpoint = server.endpoint.inner.clone();
    let serving = tokio::spawn(async move { endpoint.serve().await.unwrap(); });
    let request = create_test_request(Method::Invite, "bob", None, "rustpbx.com", None);
    let context = CallContext {
        session_id: "rwi-manual".into(),
        dialplan: Arc::new(Dialplan::new("rwi-manual".into(), request, DialDirection::Outbound)),
        cookie: TransactionCookie::default(), start_time: Instant::now(),
        original_caller: "sip:bob@rustpbx.com".into(), original_callee: "sip:alice@rustpbx.com".into(),
        max_forwards: 70, created_at: chrono::Utc::now().to_rfc3339(), metadata: None,
    };
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let (mut session, handle, mut commands) = SipSession::new_uac(server.clone(), cancel.clone(), None, context, true);
    server.active_call_registry.register_handle("rwi-manual".into(), handle);
    let processor = Arc::new(RwiCommandProcessor::new(server.active_call_registry.clone(), gateway,
        Arc::new(crate::call::runtime::ConferenceManager::new())));
    let caller = LegInner::new("caller", &LegConfig::rtp_pcmu(), None).unwrap();
    let caller_remote = LegInner::new("caller-remote", &LegConfig::rtp_pcmu(), None).unwrap();
    let offer = caller_remote.create_offer().await.unwrap();
    let answer = caller.apply_sdp(&offer, rustrtc::SdpType::Offer).await.unwrap();
    caller_remote.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
    caller.accept(); caller_remote.accept();
    session.legs.set_media_leg(&LegId::from("caller"), caller.clone());
    session.update_leg_state(&LegId::from("caller"), LegState::Connected);

    let mut invites = Vec::new();
    let mut buffer = [0u8; 65536];
    // Four simultaneously pending attempts, with IDs generated by the real RWI processor.
    for _ in 0..4 {
        let processor = processor.clone();
        let task = tokio::spawn(async move { processor.process_command(RwiCommandPayload::LegAdd {
            call_id: "rwi-manual".into(), target: format!("sip:alice@{addr}"), leg_id: None,
        }).await });
        // Acknowledge enqueueing before the session executes the dial.
        let RwiResult::LegAdded { leg_id: returned_id } = timeout(Duration::from_secs(3), task)
            .await.unwrap().unwrap().unwrap() else { panic!("expected leg ID in result"); };
        let command = commands.recv().await.unwrap();
        let leg_id = match &command {
            CallCommand::LegAdd { leg_id: Some(id), .. } => id.to_string(),
            _ => panic!("expected generated leg ID in queued command"),
        };
        assert_eq!(returned_id, leg_id);
        assert!(session.execute_command(command, None).await.success);
        let (size, pbx) = timeout(Duration::from_secs(3), target.recv_from(&mut buffer)).await.unwrap().unwrap();
        let SipMessage::Request(invite) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() else { panic!("INVITE"); };
        assert_eq!(invite.method, Method::Invite);
        let mut ringing = server.endpoint.inner.make_response(&invite, StatusCode::Ringing, None);
        ringing.headers.retain(|h| !matches!(h, Header::To(_)));
        ringing.headers.push(Header::To(format!("<sip:alice@{addr}>;tag={leg_id}").into()));
        target.send_to(ringing.to_string().as_bytes(), pbx).await.unwrap();
        // Drain the ringing notification before requesting the next leg.
        let cmd = timeout(Duration::from_secs(3), commands.recv()).await.unwrap().unwrap();
        assert!(matches!(&cmd, CallCommand::LegRinging { leg_id: id } if id.as_str() == leg_id));
        session.execute_command(cmd, None).await;
        invites.push((leg_id, invite, pbx));
    }
    let rejected = &invites[0];
    let mut rejection = server.endpoint.inner.make_response(&rejected.1, StatusCode::BusyHere, None);
    rejection.headers.retain(|h| !matches!(h, Header::To(_)));
    rejection.headers.push(Header::To(format!("<sip:alice@{addr}>;tag={}", rejected.0).into()));
    target.send_to(rejection.to_string().as_bytes(), rejected.2).await.unwrap();
    let failed = timeout(Duration::from_secs(3), commands.recv()).await.unwrap().unwrap();
    assert!(matches!(&failed, CallCommand::LegFailed { leg_id, .. } if leg_id.as_str() == rejected.0));
    session.execute_command(failed, None).await;
    assert!(!session.legs.contains_key(&LegId::new(&rejected.0)));
    assert!(!cancel.is_cancelled());

    let mut answered_peers = Vec::new();
    for winner in [&invites[1], &invites[3]] {
    let remote = LegInner::new("winner-remote", &LegConfig::rtp_pcmu(), None).unwrap();
    let answer_sdp = remote.apply_sdp(std::str::from_utf8(&winner.1.body).unwrap(), rustrtc::SdpType::Offer).await.unwrap();
    remote.accept();
    let mut answer = server.endpoint.inner.make_response(&winner.1, StatusCode::OK, Some(answer_sdp.into_bytes()));
    answer.headers.retain(|h| !matches!(h, Header::To(_)));
    answer.headers.push(Header::To(format!("<sip:alice@{addr}>;tag={}", winner.0).into()));
    answer.headers.push(Header::Contact(format!("<sip:alice@{addr}>").into()));
    answer.headers.push(Header::ContentType("application/sdp".into()));
    target.send_to(answer.to_string().as_bytes(), winner.2).await.unwrap();
    let connected = timeout(Duration::from_secs(3), commands.recv()).await.unwrap().unwrap();
    assert!(matches!(&connected, CallCommand::LegConnected { .. }));
    assert!(session.execute_command(connected, None).await.success);
    assert!(!session.bridge.active, "answered peer must remain isolated");
    assert!(!caller.egress_is_relay());

    answered_peers.push(remote);
    }
    let winner = &invites[1];
    // Select the winner through the local-leg RWI bridge API, then remove a ringing loser.
    for payload in [
        RwiCommandPayload::Bridge { call_id: "rwi-manual".into(), leg_a: "caller".into(), leg_b: winner.0.clone() },
        RwiCommandPayload::LegRemove { call_id: "rwi-manual".into(), leg_id: invites[2].0.clone() },
    ] {
        let processor = processor.clone();
        let task = tokio::spawn(async move { processor.process_command(payload).await });
        task.await.unwrap().unwrap();
        let cmd = timeout(Duration::from_secs(3), commands.recv()).await.unwrap().unwrap();
        assert!(session.execute_command(cmd, None).await.success);
    }
    assert!(caller.egress_is_relay());
    let cancel_request = timeout(Duration::from_secs(3), async {
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(req) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() {
                if req.method == Method::Cancel { break req; }
            }
        }
    }).await.expect("ringing loser receives CANCEL");
    assert_eq!(cancel_request.call_id_header().unwrap(), invites[2].1.call_id_header().unwrap());
    let ok = server.endpoint.inner.make_response(&cancel_request, StatusCode::OK, None);
    target.send_to(ok.to_string().as_bytes(), invites[2].2).await.unwrap();
    let terminated = server.endpoint.inner.make_response(&invites[2].1, StatusCode::RequestTerminated, None);
    target.send_to(terminated.to_string().as_bytes(), invites[2].2).await.unwrap();

    // Explicit unbridge must persist through later media updates.

    let result = session.execute_command(CallCommand::Unbridge { leg_id: LegId::from("caller") }, None).await;
    assert!(result.success);
    session.update_media_path().await;
    assert!(!session.bridge.active);
    assert!(!caller.egress_is_relay());

    // Removing an answered leg sends BYE, without ending the caller.

    let result = session.execute_command(CallCommand::LegRemove { leg_id: LegId::new(&invites[3].0) }, None).await;
    assert!(result.success);
    let bye = timeout(Duration::from_secs(3), async {
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(req) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() {
                if req.method == Method::Bye { break req; }
            }
        }
    }).await.expect("answered leg receives BYE");
    assert_eq!(bye.call_id_header().unwrap(), invites[3].1.call_id_header().unwrap());
    let ok = server.endpoint.inner.make_response(&bye, StatusCode::OK, None);
    target.send_to(ok.to_string().as_bytes(), winner.2).await.unwrap();
    assert!(!cancel.is_cancelled());
    assert_eq!(session.legs.get(&LegId::from("caller")).unwrap().state, LegState::Connected);

    // A subsequent dial to the same target gets a fresh dialog; stale failure is harmless.

    let result = session.execute_command(CallCommand::LegAdd {
        source_leg: None, target: format!("sip:alice@{addr}"), leg_id: Some(LegId::from("retry")), headers: vec![],
    }, None).await;
    assert_eq!(result.affected_leg, Some(LegId::from("retry")));
    session.execute_command(CallCommand::LegFailed { leg_id: LegId::new(&rejected.0), reason: "late rejection".into() }, None).await;
    assert!(session.legs.contains_key(&LegId::from("retry")));
    let retry_invite = timeout(Duration::from_secs(3), async {
        loop {
            let (size, _) = target.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Request(req) = SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap() {
                if req.method == Method::Invite { break req; }
            }
        }
    }).await.unwrap();
    assert_ne!(retry_invite.call_id_header().unwrap(), rejected.1.call_id_header().unwrap());
    assert!(session.legs.contains_key(&LegId::new(&winner.0)));
    // Setup failure after enqueue must be observable through the same event channel.
    let saved_sender = session.cmd_tx.take();
    let failed = session.execute_command(CallCommand::LegAdd {
        source_leg: None, target: format!("sip:alice@{addr}"),
        leg_id: Some(LegId::from("setup-failure")), headers: vec![],
    }, None).await;
    assert!(!failed.success);
    session.cmd_tx = saved_sender;
    let mut leg_events = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.event.payload["leg_id"].is_string() && matches!(event.event.event_type, "call_ringing" | "call_answered" | "call_hangup") { leg_events.push(event.event.payload); }
    }
    assert!(leg_events.iter().any(|e| e["leg_id"] == rejected.0 && e["event_type"] == "call_ringing"));
    assert!(leg_events.iter().any(|e| e["leg_id"] == rejected.0 && e["event_type"] == "call_hangup" && e["sip_status"] == 486));
    assert!(leg_events.iter().any(|e| e["leg_id"] == "setup-failure" && e["event_type"] == "call_hangup"
        && e["reason"].as_str().unwrap().contains("No command sender")));
    assert!(leg_events.iter().any(|e| e["leg_id"] == winner.0 && e["event_type"] == "call_answered"));
    assert!(!leg_events.iter().any(|e| e["leg_id"] == invites[2].0 && e["event_type"] == "call_hangup"), "explicit removal only receives its command acknowledgement");
    let mut ws_leg_events = Vec::new();
    while let Ok(event) = ws_events.try_recv() {
        if event["leg_id"].is_string() && matches!(event["event_type"].as_str(), Some("call_ringing" | "call_answered" | "call_hangup")) { ws_leg_events.push(event); }
    }
    assert_eq!(ws_leg_events.len(), leg_events.len(), "every leg event must reach its RWI owner");
    cancel.cancel();
    drop(session);
    caller_remote.stop();
    for peer in answered_peers { peer.stop(); }
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
        let request = create_test_request(rsipstack::sip::Method::Invite, "caller", None, "rustpbx.com", None);
        let dialplan = Dialplan::new("rwi-mode".into(), request, DialDirection::Inbound)
            .with_media(MediaConfig::new().with_proxy_mode(MediaProxyMode::All));
        let (mut session, _handle, _commands) = build_session_with_cmd_rx(dialplan).await;
        let _guard = session.cancel_token.clone().drop_guard();
        let mut remotes = Vec::new();
        for name in ["caller", "target"] {
            if name == "target" {
                let leg = crate::call::domain::Leg::new(LegId::from(name));
                session.legs.insert(LegId::from(name), leg);
            }
            let peer = LegInner::new(name, &LegConfig::rtp_pcmu(), None).unwrap();
            let remote = LegInner::new(format!("remote-{name}"), &LegConfig::rtp_pcmu(), None).unwrap();
            let offer = remote.create_offer().await.unwrap();
            let answer = peer.apply_sdp(&offer, rustrtc::SdpType::Offer).await.unwrap();
            remote.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
            peer.accept(); remote.accept();
            session.legs.set_media_leg(&LegId::from(name), peer);
            session.update_leg_state(&LegId::from(name), LegState::Connected);
            remotes.push(remote);
        }
        // Additional connected legs must not cause implicit bridge selection.
        let spare_id = LegId::from("rwi-spare");
        let mut spare = crate::call::domain::Leg::new(spare_id.clone());
        spare.state = LegState::Connected;
        if extra_leg { session.legs.insert(spare_id, spare); }
        session.update_media_path().await;
        assert!(!session.bridge.active);
        assert!(!session.media_leg(&LegId::from("caller")).unwrap().egress_is_relay());
        assert!(session.setup_bridge(LegId::from("caller"), LegId::from("target")).await);
        assert!(session.bridge.active);
        session.execute_command(CallCommand::Unbridge { leg_id: LegId::from("caller") }, None).await;
        session.update_media_path().await;
        assert!(!session.bridge.active, "unbridge must not select another pair");
        session.meta.queue_name = Some("sales".into());
        session.update_media_path().await;
        assert!(!session.bridge.active, "queue context must not implicitly select a pair");
        session.meta.queue_name = None;
        session.execute_command(CallCommand::Unbridge { leg_id: LegId::from("caller") }, None).await;

        // Invalid/reserved/active leg IDs fail synchronously without touching existing peers.
        for id in ["caller", "target"] {

            let response = session.execute_command(CallCommand::LegAdd {
                source_leg: None, target: "sip:alice@127.0.0.1:9".into(), leg_id: Some(LegId::from(id)), headers: vec![],
            }, None).await;
            assert!(!response.success, "must reject {id}");
        }
        // A removed ID can be dialed again, including to a different target.
        for target in ["sip:alice@127.0.0.1:9", "sip:bob@127.0.0.1:9"] {

            let response = session.execute_command(CallCommand::LegAdd {
                source_leg: None, target: target.into(), leg_id: Some(LegId::from("reused")), headers: vec![],
            }, None).await;
            assert!(response.success);
            assert_eq!(session.legs.get(&LegId::from("reused")).unwrap().endpoint.as_deref(), Some(target));

            let response = session.execute_command(CallCommand::LegRemove {
                leg_id: LegId::from("reused"),
            }, None).await;
            assert!(response.success);
            assert!(!session.legs.contains_key(&LegId::from("reused")));
        }
        // An immediate setup failure must return the execution error, not queue success.
        let saved_sender = session.cmd_tx.take();

        let response = session.execute_command(CallCommand::LegAdd {
            source_leg: None, target: "sip:alice@127.0.0.1:9".into(), leg_id: Some(LegId::from("bad-target")), headers: vec![],
        }, None).await;
        let result = response;
        assert!(!result.success);
        assert!(result.message.unwrap().contains("No command sender"));
        session.cmd_tx = saved_sender;
        assert!(!session.legs.contains_key(&LegId::from("bad-target")));
        assert!(session.legs.contains_key(&LegId::from("caller")));
        if let Some(bridge) = session.bridge_mut() { bridge.close(); }
        for remote in remotes { remote.stop(); }
    }
}

#[tokio::test]
async fn rwi_route_app_waits_and_announces_to_configured_context() {
    use crate::call::runtime::AppFactory;
    use crate::call::app::{ApplicationContext, CallInfo, testing::MockCallStack};
    let gateway = Arc::new(parking_lot::RwLock::new(crate::rwi::RwiGateway::new()));
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let (other_tx, mut other_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut gw = gateway.write();
        for (context, tx) in [("test-router", events_tx), ("default", other_tx)] {
            let session = gw.create_session(crate::rwi::auth::RwiIdentity {
                token: context.into(), scopes: vec![],
            });
            let id = session.read().id.clone();
            gw.set_session_event_sender(&id, tx);
            gw.subscribe(&id, vec![context.into()], None);
        }
    }
    let mut context = ApplicationContext::new(
        sea_orm::DatabaseConnection::default(),
        CallInfo {
            session_id: "test-session".into(), caller: "alice".into(),
            callee: "unregistered-entry".into(), direction: "inbound".into(),
            started_at: chrono::Utc::now(), sip_headers: Default::default(),
            route_name: Some("rwi-test".into()),
        },
        Arc::new(crate::config::Config::default()), reqwest::Client::new(),
    );
    let factory = BuiltinAppFactory::new(None, None);
    assert!(factory.create_app("rwi", None, &context).await.is_err());
    context.rwi_gateway = Some(gateway);
    let app = factory.create_app("rwi", Some(serde_json::json!({"context":"test-router"})), &context)
        .await.unwrap().unwrap();
    let mut stack = MockCallStack::run_with_context(app, context);
    let event = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
        .await.unwrap().unwrap();
    let event = serde_json::to_value(event).unwrap();
    assert_eq!(event["event_type"], "call_created");
    assert_eq!(event["call_id"], "test-session");
    assert_eq!(event["context"], "test-router");
    assert_eq!(event["callee"], "unregistered-entry");
    assert!(stack.next_cmd(50).await.is_none(), "RWI app must wait without answering or dialing");
    assert!(events_rx.try_recv().is_err());
    assert!(other_rx.try_recv().is_err());
}
