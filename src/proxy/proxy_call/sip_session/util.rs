use super::prelude::*;

pub type CalleeError = (u16, String, Option<String>);

pub(super) fn normalize_call_hangup_by(
    hangup_by: &str,
    queue_name: Option<&str>,
    has_resolved_agent: bool,
) -> String {
    if hangup_by == "agent" && queue_name.is_none() && !has_resolved_agent {
        "callee".to_string()
    } else {
        hangup_by.to_string()
    }
}

pub(super) fn sip_status_to_hangup_reason(status_code: u16) -> CallRecordHangupReason {
    match status_code {
        486 | 600 => CallRecordHangupReason::Rejected,
        487 => CallRecordHangupReason::Canceled,
        408 => CallRecordHangupReason::NoAnswer,
        480 | 484 | 485 => CallRecordHangupReason::NoAnswer,
        481 | 482 | 483 => CallRecordHangupReason::Failed,
        488 | 489 => CallRecordHangupReason::Failed,
        491 | 493 => CallRecordHangupReason::Failed,
        500 | 502 | 503 => CallRecordHangupReason::ServerUnavailable,
        504 => CallRecordHangupReason::ServerUnavailable,
        603 => CallRecordHangupReason::Rejected,
        604 => CallRecordHangupReason::NoAnswer,
        _ if (400..500).contains(&status_code) => CallRecordHangupReason::Failed,
        _ if (500..600).contains(&status_code) => CallRecordHangupReason::ServerUnavailable,
        _ if (600..700).contains(&status_code) => CallRecordHangupReason::Failed,
        _ => CallRecordHangupReason::Failed,
    }
}

pub(super) fn format_duration_ms(ms: i64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}ms", ms)
    }
}

pub fn into_callee_err(code: &StatusCode, msg: Option<String>) -> CalleeError {
    (code.code(), code.text().to_string(), msg)
}

/// Percent-decode a query-string value (replace `+` with space, then URL-decode).
pub(crate) fn pct_decode_query(value: &str) -> String {
    let s = value.replace('+', " ");
    match urlencoding::decode(&s) {
        Ok(c) => c.into_owned(),
        Err(_) => s,
    }
}

/// Route an app/transfer/RWI-originated call target through the route table.
pub(crate) async fn route_outbound_leg(
    server: &SipServerRef,
    target_uri: &rsipstack::sip::Uri,
    caller: &rsipstack::sip::Uri,
    contact: &rsipstack::sip::Uri,
    carry_headers: Option<Vec<rsipstack::sip::Header>>,
    cookie: crate::call::cookie::TransactionCookie,
) -> Result<Option<crate::config::RouteResult>> {
    route_leg(
        server,
        target_uri,
        caller,
        contact,
        carry_headers,
        &crate::call::DialDirection::Outbound,
        cookie,
    )
    .await
}

pub(crate) async fn route_leg(
    server: &SipServerRef,
    target_uri: &rsipstack::sip::Uri,
    caller: &rsipstack::sip::Uri,
    contact: &rsipstack::sip::Uri,
    carry_headers: Option<Vec<rsipstack::sip::Header>>,
    direction: &crate::call::DialDirection,
    cookie: crate::call::cookie::TransactionCookie,
) -> Result<Option<crate::config::RouteResult>> {
    use crate::call::RouteInvite;

    let route_invite: Box<dyn RouteInvite> = {
        let routing_state = server.routing_state.read().clone();
        match crate::proxy::call::compose_route_invites(server, routing_state) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to create RouteInvite for originated leg");
                return Ok(None);
            }
        }
    };

    let caller_display_name = None;
    let mut headers: Vec<rsipstack::sip::Header> = Vec::new();
    headers.push(rsipstack::sip::headers::MaxForwards::from(70u32).into());
    if let Some(carry) = carry_headers {
        headers.extend(carry);
    }

    headers.push(rsipstack::sip::Header::From(
        format!("<{}>", caller)
            .try_into()
            .map_err(|e| anyhow!("invalid From header for routed leg: {:?}", e))?,
    ));
    headers.push(rsipstack::sip::Header::To(
        format!("<{}>", target_uri).into(),
    ));
    headers.push(rsipstack::sip::Header::CallId(
        rsipstack::transaction::make_call_id(
            server.endpoint.inner.option.callid_suffix.as_deref(),
            server.endpoint.inner.option.callid_format,
        )
        .value()
        .to_string()
        .into(),
    ));
    headers.push(rsipstack::sip::Header::CSeq(
        format!("20 INVITE")
            .try_into()
            .map_err(|e| anyhow!("invalid CSeq header for routed leg: {:?}", e))?,
    ));

    let synthetic_request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: target_uri.clone(),
        version: rsipstack::sip::Version::V2,
        headers: headers.into(),
        body: Vec::new(),
    };

    let option = rsipstack::dialog::invitation::InviteOption {
        caller_display_name,
        callee: target_uri.clone(),
        caller: caller.clone(),
        contact: contact.clone(),
        ..Default::default()
    };

    match route_invite
        .route_invite(option, &synthetic_request, direction, &cookie)
        .await
    {
        Ok(result) => Ok(Some(result)),
        Err(e) => {
            tracing::warn!(error = %e, target = %target_uri, "Failed to route originated leg");
            Ok(None)
        }
    }
}

pub(super) fn parse_dtmf_digit(body_text: &str) -> Option<char> {
    for line in body_text.lines() {
        let line = line.trim();
        if line.to_lowercase().starts_with("signal=") {
            let digit = line
                .trim_start_matches(|c: char| !c.eq_ignore_ascii_case(&'s'))
                .trim_start_matches("Signal=")
                .trim_start_matches("signal=")
                .trim();
            if !digit.is_empty() {
                return Some(digit.chars().next().unwrap_or_default());
            }
        }
    }
    None
}

/// Inject a DTMF digit into the running app (and notify the RWI owner when
/// the injection succeeds). Returns `true` only when a running app consumed
/// the event — `false` means no app is running (starting up / between apps /
/// none scheduled) and the caller may want to buffer the digit.
pub(super) fn inject_dtmf_into_app(
    digit: char,
    leg_id: &str,
    session_id: &str,
    app_runtime: &Arc<dyn AppRuntime>,
    rwi_gateway: &Option<crate::rwi::RwiGatewayRef>,
) -> bool {
    if !app_runtime.is_running() {
        return false;
    }
    let digit_str = digit.to_string();
    let event = serde_json::json!({
        "type": "dtmf",
        "leg_id": leg_id,
        "digit": digit_str,
    });
    match app_runtime.inject_event(event) {
        Err(e) => {
            debug!(session_id = %session_id, digit = %digit_str, error = %e,
                "DTMF app injection failed");
            false
        }
        Ok(()) => {
            debug!(session_id = %session_id, leg_id, digit = %digit_str, "DTMF injected into app");
            if let Some(gw) = rwi_gateway.as_ref() {
                let g = gw.read();
                g.send_to_owner(&crate::rwi::Dtmf {
                    call_id: session_id.to_string(),
                    digit: digit_str.clone(),
                    leg_id: Some(leg_id.to_string()),
                    extra: None,
                });
            }
            true
        }
    }
}

/// SIP headers for proxy-emitted `ivr_step_trace` entries (bridge DTMF
/// reports, compensating session_end) — same source as the step-IVR
/// executor's own trace entries: the current (or last) app invocation's
/// headers, falling back to the call-info whitelist.
pub(super) async fn trace_sip_headers(
    app_runtime: &Arc<dyn AppRuntime>,
) -> Option<std::collections::HashMap<String, String>> {
    app_runtime
        .current_app_invocation()
        .await
        .map(|i| i.sip_headers)
        .or_else(|| {
            app_runtime
                .app_context()
                .map(|c| c.call_info.sip_headers.clone())
        })
}

/// Returns `true` when the digit was accepted by the active bridge or injected
/// into a running app. Replaying a buffered digit later must use
/// [`inject_dtmf_into_app`] instead of calling this again.
///
/// While a bridge is active (`bridge_dtmf_tx` armed) the digit is also
/// 1. buffered for the return-app IVR (`bridge_dtmf_digits`), and
/// 2. reported as an `ivr_step_trace` with the originating node context
///    (`bridge_trace_context`) so consumers see `trigger.detail.digit` for
///    menu/TTS nodes executed via a bridge (consumer contract). The trace
///    carries `step_start_time` (executor-stamped via `_rst_step_start_time`,
///    falling back to the emission instant) alongside `step_end_time` —
///    consumers derive duration as `event timestamp - step_start_time`, so
///    an end without a start would order end < start.
#[allow(clippy::too_many_arguments)]
pub(super) fn forward_dtmf_event(
    digit: char,
    leg_id: &str,
    session_id: &str,
    app_runtime: &Arc<dyn AppRuntime>,
    rwi_gateway: &Option<crate::rwi::RwiGatewayRef>,
    bridge_dtmf_tx: &Arc<parking_lot::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
    bridge_trace_context: &parking_lot::Mutex<Option<super::transfer::BridgeTraceContext>>,
    bridge_dtmf_digits: &parking_lot::Mutex<Vec<String>>,
    caller: &str,
    callee: &str,
    sip_headers: Option<std::collections::HashMap<String, String>>,
) -> bool {
    let digit_str = digit.to_string();
    if let Some(tx) = bridge_dtmf_tx.read().as_ref() {
        let _ = tx.send(
            serde_json::json!({
                "type": "dtmf",
                "digit": digit_str,
                "leg_id": leg_id,
            })
            .to_string(),
        );
        // Bridge active: buffer + report.
        bridge_dtmf_digits.lock().push(digit_str.clone());
        let ctx = bridge_trace_context.lock().clone();
        if let Some(gw) = rwi_gateway.as_ref()
            && let Some(ctx) = ctx
        {
            // Single clock capture: RFC3339 carries nanosecond precision, so
            // two Utc::now() calls would differ and could order end < start.
            // The executor-stamped start time (via `_rst_step_start_time`)
            // keeps `step_end_time >= step_start_time` meaningful; `now` is
            // only the fallback for bridges without node context.
            let now = chrono::Utc::now().to_rfc3339();
            let step_start = ctx.step_start_time.clone().unwrap_or_else(|| now.clone());
<<<<<<< Updated upstream
            let duration_ms =
                crate::call::app::ivr::trace::duration_ms_between(Some(&step_start), &now);
=======
            let duration_ms = crate::call::app::ivr::trace::duration_ms_between(
                Some(&step_start),
                &now,
            );
>>>>>>> Stashed changes
            let ev = crate::rwi::IvrStepTrace {
                call_id: session_id.to_string(),
                session_id: session_id.to_string(),
                caller: caller.to_string(),
                callee: callee.to_string(),
                trigger: crate::rwi::TriggerInfo::with_detail(
                    "dtmf",
                    serde_json::json!({ "digit": digit_str }),
                ),
                action_type: "Bridge".to_string(),
                action_json: None,
                duration_ms,
                error: None,
                step_id: ctx.step_id,
                step_name: ctx.step_name,
                step_start_time: Some(step_start),
                step_end_time: Some(now),
                extra: ctx.extra,
                sip_headers,
                end_reason: None,
                end_detail: None,
            };
            gw.read().fan_out(session_id, &ev);
        }
        // The bridge owns media while active. Injecting the same digit into
        // the suspended app would replay one physical key as a second input.
        return true;
    }
    inject_dtmf_into_app(digit, leg_id, session_id, app_runtime, rwi_gateway)
}

/// Map the session teardown cause to the compensating `session_end` reason
/// for an IVR flow that died while suspended on a resumable hand-off.
/// Caller-side death stays `user_hangup`; the RTP watchdog refines to
/// `timeout`; everything else is a system teardown (`hangup`) with the CDR
/// hangup reason as detail.
pub(super) fn map_suspended_flow_end(
    reason: Option<&CallRecordHangupReason>,
) -> crate::call::app::ivr::provider::SessionEndReason {
    use crate::call::app::ivr::provider::{SessionEndReason, SessionEndTag};
    use CallRecordHangupReason as R;
    match reason {
        Some(R::ByCaller) | Some(R::Canceled) | Some(R::Abandoned) | Some(R::NoAnswer) => {
            SessionEndReason {
                reason: SessionEndTag::UserHangup,
                detail: None,
            }
        }
        Some(R::RtpTimeout) => SessionEndReason {
            reason: SessionEndTag::Timeout,
            detail: None,
        },
        Some(other) => SessionEndReason {
            reason: SessionEndTag::Hangup,
            detail: Some(format!("{other:?}")),
        },
        None => SessionEndReason {
            reason: SessionEndTag::Hangup,
            detail: None,
        },
    }
}

/// Emit the compensating `session_end` `ivr_step_trace` for an IVR flow that
/// died while suspended (caller hangup during a bridge/queue hand-off, or a
/// JumpIvr target that failed to start). The step-IVR executor suppresses its
/// own session_end trace for resumable hand-offs, so this synthetic event
/// guarantees consumers still see exactly one session_end per logical flow —
/// carrying the REAL end reason instead of a premature `transfer`. Node
/// context comes from the bridge trace context when the suspension was a
/// voip_bridge.
pub(super) fn emit_suspended_flow_session_end(
    session_id: &str,
    caller: &str,
    callee: &str,
    rwi_gateway: &Option<crate::rwi::RwiGatewayRef>,
    bridge_trace_context: &parking_lot::Mutex<Option<super::transfer::BridgeTraceContext>>,
    sip_headers: Option<std::collections::HashMap<String, String>>,
    end_reason: crate::call::app::ivr::provider::SessionEndReason,
) {
    let Some(gw) = rwi_gateway.as_ref() else {
        return;
    };
    let ctx = bridge_trace_context.lock().clone();
    // Single clock capture (see forward_dtmf_event): one `now` serves as the
    // end stamp and as the start fallback so `end >= start` always holds.
    // The executor-stamped `_rst_step_start_time` provides the real start.
    let now = chrono::Utc::now().to_rfc3339();
    let step_start = ctx
        .as_ref()
        .and_then(|c| c.step_start_time.clone())
        .unwrap_or_else(|| now.clone());
<<<<<<< Updated upstream
    let duration_ms = crate::call::app::ivr::trace::duration_ms_between(Some(&step_start), &now);
=======
    let duration_ms =
        crate::call::app::ivr::trace::duration_ms_between(Some(&step_start), &now);
>>>>>>> Stashed changes
    let ev = crate::rwi::IvrStepTrace {
        call_id: session_id.to_string(),
        session_id: session_id.to_string(),
        caller: caller.to_string(),
        callee: callee.to_string(),
        trigger: crate::rwi::TriggerInfo::new("session_end"),
        action_type: if ctx.is_some() { "Bridge" } else { "Transfer" }.to_string(),
        action_json: None,
        duration_ms,
        error: None,
        step_id: ctx.as_ref().and_then(|c| c.step_id.clone()),
        step_name: ctx.as_ref().and_then(|c| c.step_name.clone()),
        step_start_time: Some(step_start),
        step_end_time: Some(now),
        extra: ctx.as_ref().and_then(|c| c.extra.clone()),
        sip_headers,
        end_reason: Some(end_reason.reason),
        end_detail: end_reason.detail,
    };
    gw.read().fan_out(session_id, &ev);
}

pub(super) fn trunk_host_port(dest: &str) -> Option<(String, u16)> {
    if dest.trim().is_empty() {
        return None;
    }
    if let Ok(uri) = rsipstack::sip::Uri::try_from(dest) {
        let host = uri.host().to_string();
        let port = uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);
        return Some((host, port));
    }
    let parts: Vec<&str> = dest.split(':').collect();
    let host = *parts.first()?;
    if host.is_empty() {
        return None;
    }
    let port = parts
        .get(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5060);
    Some((host.to_string(), port))
}

pub(super) fn parse_dial_target(target: &str) -> Result<rsipstack::sip::Uri> {
    let trimmed = target.trim();
    if let Ok(uri) = rsipstack::sip::Uri::try_from(trimmed) {
        return Ok(uri);
    }
    rsipstack::sip::typed::Contact::parse(trimmed)
        .map(|c| c.uri)
        .map_err(|e| anyhow!("invalid SIP target '{}': {}", target, e))
}

pub(super) fn other_header_ci(
    headers: &[rsipstack::sip::Header],
    names: &[&str],
) -> Option<String> {
    for h in headers {
        if let rsipstack::sip::Header::Other(n, v) = h {
            if names.iter().any(|w| n.eq_ignore_ascii_case(w)) {
                return Some(v.clone());
            }
        }
    }
    None
}

pub(super) fn parse_sipfrag_status(body: &str) -> Option<u16> {
    let line = body.lines().next()?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 && parts[0] == "SIP/2.0" {
        parts[1].parse().ok()
    } else {
        None
    }
}
