//! Wire-format regression guard for the unified `RwiCommandPayload` enum.
//!
//! `RwiCommandPayload` doubles as the internal command type and the
//! WebSocket wire type (tag = "action", content = "params"). Every command
//! name below MUST keep deserializing — this catches rename/alias typos and
//! missing `#[serde(default)]`s at the source level, before the e2e suite.

use rustpbx::rwi::session::{OwnershipMode, RwiRequest};

fn parse(action: &str, params: serde_json::Value) -> Result<String, String> {
    let json = serde_json::json!({
        "action": action,
        "action_id": "test-action",
        "params": params,
    });
    let req: RwiRequest = serde_json::from_value(json).map_err(|e| format!("{action}: {e}"))?;
    let _ = req.action_id;
    Ok(String::new())
}

#[test]
fn all_wire_command_names_deserialize() {
    // (action, minimal params). `{}` works unless the variant carries
    // required fields.
    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("session.subscribe", serde_json::json!({"contexts": []})),
        ("Subscribe", serde_json::json!({"contexts": []})),
        ("session.unsubscribe", serde_json::json!({"contexts": []})),
        ("session.attach_call", serde_json::json!({})),
        ("session.detach_call", serde_json::json!({})),
        ("session.list_calls", serde_json::json!(null)),
        ("session.resume", serde_json::json!({})),
        ("call.originate", serde_json::json!({})),
        ("Originate", serde_json::json!({})),
        ("call.answer", serde_json::json!({})),
        ("call.reject", serde_json::json!({})),
        ("call.ring", serde_json::json!({})),
        ("call.hangup", serde_json::json!({})),
        ("call.bridge", serde_json::json!({})),
        ("call.unbridge", serde_json::json!({})),
        ("call.transfer", serde_json::json!({})),
        ("call.transfer.replace", serde_json::json!({})),
        ("call.transfer.attended", serde_json::json!({})),
        ("call.transfer.complete", serde_json::json!({})),
        ("call.transfer.cancel", serde_json::json!({})),
        ("call.hold", serde_json::json!({})),
        ("call.unhold", serde_json::json!({})),
        ("call.set_ringback_source", serde_json::json!({})),
        ("call.set_var", serde_json::json!({})),
        ("call.get_var", serde_json::json!({})),
        ("call.send_dtmf", serde_json::json!({})),
        ("call.leg_add", serde_json::json!({})),
        ("call.leg_remove", serde_json::json!({})),
        ("call.app_start", serde_json::json!({})),
        ("call.app_stop", serde_json::json!({})),
        ("call.resume", serde_json::json!({})),
        ("app.chain", serde_json::json!({})),
        ("media.play", serde_json::json!({})),
        ("MediaPlay", serde_json::json!({})),
        ("media.stop", serde_json::json!({})),
        (
            "dtmf.collect",
            serde_json::json!({
                "min_digits": 1, "max_digits": 1, "first_digit_timeout_ms": 5000, "inter_digit_timeout_ms": 3000
            }),
        ),
        ("record.start", serde_json::json!({})),
        ("record.pause", serde_json::json!({})),
        ("record.resume", serde_json::json!({})),
        ("record.stop", serde_json::json!({})),
        ("queue.enqueue", serde_json::json!({})),
        ("queue.dequeue", serde_json::json!({})),
        ("queue.hold", serde_json::json!({})),
        ("queue.unhold", serde_json::json!({})),
        ("queue.set_priority", serde_json::json!({})),
        ("queue.assign_agent", serde_json::json!({})),
        ("queue.requeue", serde_json::json!({})),
        ("supervisor.listen", serde_json::json!({})),
        ("supervisor.whisper", serde_json::json!({})),
        ("supervisor.barge", serde_json::json!({})),
        ("supervisor.takeover", serde_json::json!({})),
        ("supervisor.stop", serde_json::json!({})),
        ("sip.message", serde_json::json!({})),
        ("sip.notify", serde_json::json!({})),
        ("sip.options_ping", serde_json::json!({})),
        ("conference.create", serde_json::json!({})),
        ("conference.add", serde_json::json!({})),
        ("conference.remove", serde_json::json!({})),
        ("conference.mute", serde_json::json!({})),
        ("conference.unmute", serde_json::json!({})),
        ("conference.destroy", serde_json::json!({"conf_id": "c1"})),
        ("conference.end", serde_json::json!({})),
        ("conference.merge", serde_json::json!({})),
        ("conference.seat_replace", serde_json::json!({})),
    ];

    for (action, params) in cases {
        if let Err(e) = parse(action, params) {
            panic!("{e}");
        }
    }
}

#[test]
fn wire_defaults_match_legacy_conversion() {
    // Missing fields default like the old unwrap_or_default() conversion.
    let json = serde_json::json!({
        "action": "call.answer",
        "action_id": "a1",
        "params": {},
    });
    let req: RwiRequest = serde_json::from_value(json).unwrap();
    match req.payload {
        rustpbx::rwi::session::RwiCommandPayload::Answer { call_id } => {
            assert_eq!(call_id, "");
        }
        other => panic!("wrong variant: {other:?}"),
    }

    // AttachCall mode mapping (unknown → Control).
    let json = serde_json::json!({
        "action": "session.attach_call",
        "action_id": "a2",
        "params": {"call_id": "c1", "mode": "listen"},
    });
    let req: RwiRequest = serde_json::from_value(json).unwrap();
    match req.payload {
        rustpbx::rwi::session::RwiCommandPayload::AttachCall { call_id, mode } => {
            assert_eq!(call_id, "c1");
            assert_eq!(mode, OwnershipMode::Listen);
        }
        other => panic!("wrong variant: {other:?}"),
    }

    // SipMessage content_type default.
    let json = serde_json::json!({
        "action": "sip.message",
        "action_id": "a3",
        "params": {"call_id": "c1", "body": "hi"},
    });
    let req: RwiRequest = serde_json::from_value(json).unwrap();
    match req.payload {
        rustpbx::rwi::session::RwiCommandPayload::SipMessage {
            content_type, body, ..
        } => {
            assert_eq!(content_type, "text/plain");
            assert_eq!(body, "hi");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn normalize_generates_missing_ids() {
    let json = serde_json::json!({
        "action": "call.originate",
        "action_id": "a4",
        "params": {"destination": "sip:1001@x"},
    });
    let mut req: RwiRequest = serde_json::from_value(json).unwrap();
    req.payload.normalize();
    match req.payload {
        rustpbx::rwi::session::RwiCommandPayload::Originate(r) => {
            assert!(!r.call_id.is_empty(), "call_id must be generated");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
