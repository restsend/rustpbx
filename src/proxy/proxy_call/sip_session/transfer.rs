use super::SipSession;
use crate::call::domain::{CallCommand, LegId, LegState, ReturnAppSpec};
use crate::media::negotiate::MediaNegotiator;
use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use rsipstack::dialog::dialog::DialogState;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

// Re-export for peer access
use rustrtc::PeerConnection;
use rustrtc::media::SampleStreamSource;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TransferDisposition {
    Detach,
    AwaitResult,
}

fn use_b2bua(blind_transfer_use_refer: bool, disposition: TransferDisposition) -> bool {
    !blind_transfer_use_refer || disposition == TransferDisposition::AwaitResult
}

async fn wait_for_bridge_disconnect(
    session_cancel: tokio_util::sync::CancellationToken,
    bridge_cancel: tokio_util::sync::CancellationToken,
    mut forward_handle: tokio::task::JoinHandle<()>,
    mut reverse_handle: tokio::task::JoinHandle<()>,
    mut pcm_ended_rx: Option<tokio::sync::oneshot::Receiver<()>>,
) -> bool {
    enum CompletedTask {
        Session,
        Forward,
        Reverse,
    }

    let completed = tokio::select! {
        biased;
        _ = session_cancel.cancelled() => CompletedTask::Session,
        _ = &mut forward_handle => CompletedTask::Forward,
        _ = &mut reverse_handle => CompletedTask::Reverse,
    };

    match completed {
        CompletedTask::Session => {
            bridge_cancel.cancel();
            let _ = forward_handle.await;
            let _ = reverse_handle.await;
        }
        CompletedTask::Forward => {
            // Drain ChannelAudioSource before cancelling reverse so return-app
            // does not cut off the PCM tail / leave a CNG gap in recordings.
            if let Some(rx) = pcm_ended_rx.take() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
            }
            bridge_cancel.cancel();
            let _ = reverse_handle.await;
        }
        CompletedTask::Reverse => {
            bridge_cancel.cancel();
            let _ = forward_handle.await;
            if let Some(rx) = pcm_ended_rx.take() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
            }
        }
    }
    !session_cancel.is_cancelled()
}

/// Unified forward sink for the bridge: WS PCM16 → call. Two backing paths:
/// - [`BridgeForwardSink::Track`]: a `VoiceEnginePeer` track sender (non-app
///   B2BUA path).
/// - [`BridgeForwardSink::Pcm`]: a raw-PCM channel into the MediaBridge A leg's
///   egress pipeline (app-anchored flow, e.g. IVR bridge). The egress encoder
///   handles PCM→codec conversion — same "filetrack" mode as `play_file`.
enum BridgeForwardSink {
    Track(SampleStreamSource),
    Pcm(tokio::sync::mpsc::Sender<Vec<i16>>),
}

/// Raw return-app specification extracted from a transfer target's query
/// string.  Resolved to a concrete [`ReturnAppSpec`] by the transfer handler
/// (which has access to `data_context` for IVR file resolution).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReturnTargetSpec {
    /// Application name, e.g. `"ivr"`, `"voicemail"`.
    pub app_name: String,
    /// Primary target (IVR file name, voicemail extension, …).  `None` when
    /// the app does not need one.
    pub target: Option<String>,
    /// Extra `return_*` query params (e.g. `return_menu`, `return_step_id`).
    pub params: HashMap<String, String>,
}

impl ReturnTargetSpec {
    /// Parse `return_app` / `return_target` / `return_*` query pairs from an
    /// iterator of `(key, decoded_value)`.
    ///
    /// Returns `Some(ReturnTargetSpec)` only when a `return_app` key is
    /// present and non-empty.  All other `return_*` keys are collected into
    /// `params`.
    fn from_query_pairs<'a>(pairs: impl Iterator<Item = (&'a str, String)>) -> Option<Self> {
        let mut app_name = None;
        let mut target = None;
        let mut params = HashMap::new();
        for (key, val) in pairs {
            if val.is_empty() {
                continue;
            }
            match key {
                "return_app" => app_name = Some(val),
                "return_target" => target = Some(val),
                k if k.starts_with("return_") => {
                    params.insert(k.to_string(), val);
                }
                _ => {}
            }
        }
        app_name.map(|name| Self {
            app_name: name,
            target,
            params,
        })
    }
}

/// Node identity attached by the step-IVR executor to a `bridge:` target (via
/// `_rst_*` query params) so DTMF pressed during the WebSocket bridge can be
/// reported as an `ivr_step_trace` carrying the originating node context.
/// Consumer contract: menu nodes must surface `trigger.detail.digit`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BridgeTraceContext {
    pub step_id: Option<String>,
    pub step_name: Option<String>,
    pub extra: Option<serde_json::Value>,
}

impl BridgeTraceContext {
    fn is_empty(&self) -> bool {
        self.step_id.is_none() && self.step_name.is_none() && self.extra.is_none()
    }
}

/// Parsed representation of a transfer target URI.
///
/// Extracted so that the string-prefix dispatch in `handle_blind_transfer` is
/// type-safe and unit-testable independently of `SipSession`.
#[derive(Debug, PartialEq)]
pub(crate) enum TransferTarget {
    Queue {
        name: String,
        return_app: Option<ReturnTargetSpec>,
        target_overrides: Vec<String>,
    },
    Ivr {
        name: String,
        params: HashMap<String, String>,
    },
    RoutePoint {
        name: String,
        params: HashMap<String, String>,
    },
    Voicemail {
        extension: String,
    },
    Conference {
        id: String,
    },
    /// WebSocket + PCM real-time bridge.
    Bridge {
        endpoint: String,
        headers: HashMap<String, String>,
        sample_rate: u32,
        codec: String,
        timeout_ms: Option<u64>,
        /// App to return to when the bridge disconnects.
        return_app: Option<ReturnTargetSpec>,
        /// Originating IVR node context for DTMF trace reporting.
        trace_context: Option<BridgeTraceContext>,
    },
    /// B2BUA SIP call leg, optionally with return-app on B‑leg hangup.
    Sip {
        uri: String,
        return_app: Option<ReturnTargetSpec>,
        from_user: Option<String>,
    },
}

/// Parse a raw transfer target string into a typed `TransferTarget`.
///
/// Delegates prefix dispatch to [`TransferEndpoint::parse`] and enriches the
/// result with transfer‑specific data (queue query params, voip_bridge options).
/// Bare strings without a recognised prefix get `sip:` prepended.
pub(crate) fn parse_transfer_target(target: &str) -> TransferTarget {
    // 1. `bridge:` is too complex for TransferEndpoint – parse inline.
    if let Some(rest) = target
        .strip_prefix("bridge:")
        .or_else(|| target.strip_prefix("voip_bridge:"))
    {
        let raw = rest.trim();
        if !raw.is_empty() {
            let mut sample_rate = 8000u32;
            let mut codec = "pcm".to_string();
            let mut timeout_ms = None;
            let mut return_query: Vec<(&str, String)> = Vec::new();
            let mut headers = HashMap::new();
            let mut passthrough_params = Vec::new();
            let mut trace_context = BridgeTraceContext::default();

            if let Ok(uri) = raw.parse::<http::Uri>() {
                if let Some(query) = uri.query() {
                    for pair in query.split('&') {
                        if pair.is_empty() {
                            continue;
                        }
                        let mut parts = pair.splitn(2, '=');
                        let key = parts.next().unwrap_or("");
                        let value = parts.next().unwrap_or("");
                        let decoded_val = super::pct_decode_query(value);
                        match key {
                            k if k.starts_with("_hdr_") => {
                                let hdr_name = &k["_hdr_".len()..];
                                headers.insert(hdr_name.to_string(), decoded_val);
                            }
                            "samplerate" => {
                                sample_rate = value.parse().unwrap_or(8000);
                            }
                            "codec" => {
                                codec = value.to_string();
                            }
                            "timeout_ms" => {
                                timeout_ms = value.parse().ok();
                            }
                            "return_app" | "return_target" => {
                                return_query.push((key, decoded_val));
                            }
                            k if k.starts_with("return_") => {
                                return_query.push((k, decoded_val));
                            }
                            // Reserved: originating IVR node context injected
                            // by the step executor. Never forwarded to the
                            // bridge endpoint.
                            "_rst_step_id" => trace_context.step_id = Some(decoded_val),
                            "_rst_step_name" => trace_context.step_name = Some(decoded_val),
                            "_rst_extra" => {
                                trace_context.extra = serde_json::from_str(&decoded_val).ok()
                            }
                            k if k.starts_with("_rst_") => {}
                            _ => passthrough_params.push(pair.to_string()),
                        }
                    }
                }
                let mut ep = String::new();
                if let Some(scheme) = uri.scheme_str() {
                    ep.push_str(scheme);
                    ep.push_str("://");
                }
                if let Some(auth) = uri.authority() {
                    ep.push_str(auth.as_str());
                }
                ep.push_str(uri.path());
                if !passthrough_params.is_empty() {
                    ep.push('?');
                    ep.push_str(&passthrough_params.join("&"));
                }
                return TransferTarget::Bridge {
                    endpoint: ep,
                    headers,
                    sample_rate,
                    codec,
                    timeout_ms,
                    return_app: ReturnTargetSpec::from_query_pairs(
                        return_query.into_iter().map(|(k, v)| (k.as_ref(), v)),
                    ),
                    trace_context: if trace_context.is_empty() {
                        None
                    } else {
                        Some(trace_context)
                    },
                };
            }
        }
    }

    // 2. Delegate to the canonical prefix parser.
    if let Some(ep) = crate::call::TransferEndpoint::parse(target) {
        return match ep {
            // Queue: also extract query params (return_app, target overrides).
            crate::call::TransferEndpoint::Queue(mut raw_name) => {
                let query_str = raw_name.find('?').map(|pos| {
                    let qs = raw_name[pos + 1..].to_string();
                    raw_name.truncate(pos);
                    qs
                });
                let queue_name = raw_name.trim().to_string();
                if queue_name.is_empty() {
                    TransferTarget::Sip {
                        uri: format!("sip:{}", target),
                        return_app: None,
                        from_user: None,
                    }
                } else {
                    let mut return_query: Vec<(&str, String)> = Vec::new();
                    let mut target_overrides = Vec::new();
                    if let Some(ref query) = query_str {
                        for pair in query.split('&') {
                            if pair.is_empty() {
                                continue;
                            }
                            let mut parts = pair.splitn(2, '=');
                            let key = parts.next().unwrap_or("");
                            let value = parts.next().unwrap_or("");
                            let decoded = super::pct_decode_query(value);
                            match key {
                                "target" => target_overrides.push(decoded),
                                "return_app" | "return_target" => {
                                    return_query.push((key, decoded));
                                }
                                k if k.starts_with("return_") => {
                                    return_query.push((k, decoded));
                                }
                                _ => {}
                            }
                        }
                    }
                    TransferTarget::Queue {
                        name: queue_name,
                        return_app: ReturnTargetSpec::from_query_pairs(return_query.into_iter()),
                        target_overrides,
                    }
                }
            }
            crate::call::TransferEndpoint::RoutePoint(raw_name) => {
                let (name, params) = parse_named_target(raw_name);
                TransferTarget::RoutePoint { name, params }
            }
            crate::call::TransferEndpoint::Ivr(raw_name) => {
                let (name, params) = parse_named_target(raw_name);
                TransferTarget::Ivr { name, params }
            }
            crate::call::TransferEndpoint::Voicemail(extension) => {
                TransferTarget::Voicemail { extension }
            }
            crate::call::TransferEndpoint::Conference(id) => TransferTarget::Conference { id },
            // Plain SIP/TEL URI – ensure at least the `sip:` scheme.
            // Also extract `return_app` / `return_target` / `return_*` query
            // params and strip them from the URI before it reaches the callee
            // INVITE.
            crate::call::TransferEndpoint::Uri(uri) => {
                let sip = if uri.starts_with("sip:") || uri.starts_with("tel:") {
                    uri
                } else {
                    format!("sip:{}", uri)
                };
                let mut return_query: Vec<(&str, String)> = Vec::new();
                let mut from_user = None;
                let clean_uri = if let Some(qpos) = sip.find('?') {
                    let base = &sip[..qpos];
                    let qs = &sip[qpos + 1..];
                    let mut kept = Vec::new();
                    for pair in qs.split('&') {
                        if pair.is_empty() {
                            continue;
                        }
                        let mut parts = pair.splitn(2, '=');
                        let key = parts.next().unwrap_or("");
                        let value = parts.next().unwrap_or("");
                        let decoded = super::pct_decode_query(value);
                        if key == "from_user" {
                            from_user = (!decoded.is_empty()).then_some(decoded);
                        } else if key == "return_app" || key == "return_target" {
                            return_query.push((key, decoded));
                        } else if key.starts_with("return_") {
                            return_query.push((key, decoded));
                        } else {
                            kept.push(pair);
                        }
                    }
                    if kept.is_empty() {
                        base.to_string()
                    } else {
                        format!("{}?{}", base, kept.join("&"))
                    }
                } else {
                    sip
                };
                TransferTarget::Sip {
                    uri: clean_uri,
                    return_app: ReturnTargetSpec::from_query_pairs(return_query.into_iter()),
                    from_user,
                }
            }
        };
    }

    // 3. Fallback (should not normally happen).
    TransferTarget::Sip {
        uri: format!("sip:{}", target),
        return_app: None,
        from_user: None,
    }
}

fn parse_named_target(mut raw_name: String) -> (String, HashMap<String, String>) {
    let query_str = raw_name.find('?').map(|pos| {
        let qs = raw_name[pos + 1..].to_string();
        raw_name.truncate(pos);
        qs
    });
    let name = raw_name.trim().to_string();
    let mut params = HashMap::new();
    if let Some(query) = query_str {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let mut parts = pair.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            params.insert(key.to_string(), super::pct_decode_query(value));
        }
    }
    (name, params)
}

impl SipSession {
    pub(super) async fn handle_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        attended: bool,
        disposition: TransferDisposition,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        if disposition == TransferDisposition::AwaitResult {
            self.meta.pending_transfer_outcome =
                Some(crate::call::domain::TransferOutcome::NotConnected);
        }

        let result = self
            .handle_transfer_inner(leg_id, target, attended, disposition, callee_state_rx)
            .await;
        if disposition == TransferDisposition::AwaitResult {
            if result.is_err() {
                self.deliver_pending_transfer_result();
            } else {
                self.meta.pending_transfer_outcome =
                    Some(crate::call::domain::TransferOutcome::TargetEnded);
            }
        }
        result
    }

    async fn handle_transfer_inner(
        &mut self,
        leg_id: LegId,
        target: String,
        attended: bool,
        disposition: TransferDisposition,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        let leg = self.require_leg(&leg_id)?;
        if !matches!(leg.state, LegState::Connected | LegState::Hold) {
            return Err(anyhow!(
                "Cannot transfer leg {}: invalid state {:?}",
                leg_id,
                leg.state
            ));
        }

        if attended {
            if !target.is_empty() {
                self.handle_replace_transfer(leg_id, target, callee_state_rx)
                    .await?;
            } else {
                self.update_leg_state(&leg_id, LegState::Hold);
                info!(session_id = %self.id,
                    "Attended transfer initiated - consultation call should be created externally"
                );
            }
        } else {
            self.handle_blind_transfer(leg_id, target, disposition, callee_state_rx)
                .await?;
        }

        Ok(())
    }

    /// Blind transfer: suppress the RTP-inactivity watchdog for the transfer
    /// window, then delegate to [`Self::handle_blind_transfer_inner`]. The
    /// watchdog is re-armed when the new leg answers (see
    /// `prepare_caller_answer_from_callee_sdp` / `accept_call`) or, on failure,
    /// restored immediately here.
    pub(super) async fn handle_blind_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        disposition: TransferDisposition,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        self.meta.transfer_in_progress = true;
        self.sync_rtp_timeout_pause();

        let result = self
            .handle_blind_transfer_inner(leg_id, target, disposition, callee_state_rx)
            .await;

        if result.is_err() {
            // Transfer failed — the existing bridge stays up, so normal
            // (non-suppressed) monitoring resumes.
            self.meta.transfer_in_progress = false;
            // The customer stays with the original agent, so CSAT is not
            // suppressed either.
            self.meta.transferred = false;
            self.sync_rtp_timeout_pause();
        }
        result
    }

    async fn handle_blind_transfer_inner(
        &mut self,
        leg_id: LegId,
        target: String,
        disposition: TransferDisposition,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        let target = parse_transfer_target(&target);
        if disposition == TransferDisposition::AwaitResult
            && !matches!(target, TransferTarget::Sip { .. })
        {
            return Err(anyhow!("wait_for_result requires a SIP transfer target"));
        }

        match target {
            TransferTarget::Queue {
                name,
                return_app,
                target_overrides,
            } => {
                info!(session_id = %self.id, %leg_id, queue = %name, ?return_app, overrides = %target_overrides.len(), "Handling queue transfer");
                self.handle_queue_transfer(&name, return_app, target_overrides)
                    .await
            }
            TransferTarget::Ivr { name, params } => {
                info!(session_id = %self.id, %leg_id, ivr = %name, "Handling IVR transfer by starting IvrApp");
                self.start_ivr_app(&name, params).await
            }
            TransferTarget::RoutePoint { name, params } => {
                info!(session_id = %self.id, %leg_id, route_point = %name, "Handling IVR route-point transfer");
                self.start_route_point_app(&name, params).await
            }
            TransferTarget::Voicemail { extension } => {
                info!(session_id = %self.id, %leg_id, %extension, "Handling voicemail transfer by starting VoicemailApp");
                self.start_voicemail_app(&extension).await
            }
            TransferTarget::Conference { id } => {
                info!(session_id = %self.id, %leg_id, conf_id = %id, "Handling conference transfer by starting ConferenceApp");
                self.start_conference_app(&id).await
            }
            TransferTarget::Bridge {
                endpoint,
                headers,
                sample_rate,
                codec,
                timeout_ms,
                return_app,
                trace_context,
            } => {
                info!(session_id = %self.id, %leg_id, endpoint = %endpoint, sample_rate, codec = %codec, ?return_app, ?trace_context, "Handling Bridge transfer");
                self.meta.transferred = true;
                self.connect_bridge(
                    leg_id,
                    endpoint.clone(),
                    headers.clone(),
                    sample_rate,
                    codec.clone(),
                    timeout_ms,
                    return_app.clone(),
                    trace_context.clone(),
                )
                .await
            }
            TransferTarget::Sip {
                uri,
                return_app,
                from_user,
            } => {
                self.meta.transfer_return_app = self.resolve_return_app(return_app).await;
                self.meta.transferred = true;

                let realm = self.server.proxy_config.load().select_realm("");
                let normalized = crate::call::build_sip_uri(&uri, &realm);
                let refer_to_uri = rsipstack::sip::Uri::try_from(normalized.as_str())
                    .map_err(|e| anyhow!("Invalid transfer target URI: {}", e))?;

                if use_b2bua(
                    self.server.proxy_config.load().blind_transfer_use_refer,
                    disposition,
                ) {
                    return self
                        .dial_blind_transfer_b2bua(
                            leg_id,
                            &uri,
                            &refer_to_uri,
                            from_user,
                            callee_state_rx,
                        )
                        .await;
                }

                // transfer_return_app is intentionally kept here even though
                // SIP REFER cannot carry it: if the peer rejects the REFER we
                // fall back to the B2BUA path below, which CAN honor it. It is
                // cleared once the REFER is accepted (202) or definitively
                // fails without fallback.

                let referred_by = self
                    .context
                    .dialplan
                    .caller_contact
                    .clone()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| format!("sip:{}@localhost", self.server.contact_username));
                let headers = vec![rsipstack::sip::Header::Other(
                    "Referred-By".to_string(),
                    format!("<{}>", referred_by),
                )];

                info!(session_id = %self.id, %leg_id, target = %uri, "Sending REFER for blind transfer");

                let Some(server_dialog) = self.caller_dialog.as_ref() else {
                    warn!(session_id = %self.id, "Cannot send REFER: no inbound caller dialog (UAC mode)");
                    return Err(anyhow!(
                        "REFER not supported without an inbound caller dialog; use B2BUA"
                    ));
                };
                match server_dialog
                    .refer(refer_to_uri.clone(), Some(headers), None)
                    .await
                {
                    Ok(Some(response)) => {
                        let status = response.status_code.code();
                        info!(session_id = %self.id, status = %status, "REFER response received");

                        let reason = Self::refer_reason_for_status(status).map(String::from);
                        self.emit_refer_event(
                            status,
                            reason,
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;

                        match status {
                            202 => {
                                info!(session_id = %self.id, "REFER accepted (202), transfer in progress");
                                self.meta.transfer_return_app = None;
                                self.update_leg_state(&leg_id, LegState::Ending);
                            }
                            100..=199 => {
                                info!(session_id = %self.id, status = %status, "REFER received provisional response");
                            }
                            405 | 420 | 501 => {
                                warn!(session_id = %self.id, status = %status, "REFER not supported by peer; falling back to B2BUA transfer");
                                return self
                                    .dial_blind_transfer_b2bua(
                                        leg_id,
                                        &uri,
                                        &refer_to_uri,
                                        from_user,
                                        callee_state_rx,
                                    )
                                    .await;
                            }
                            _ if status >= 400 => {
                                warn!(session_id = %self.id, status = %status, "REFER rejected");
                                self.meta.transfer_return_app = None;
                                return Err(anyhow!("REFER rejected with status {}", status));
                            }
                            _ => {
                                warn!(session_id = %self.id, status = %status, "Unexpected REFER response");
                                self.meta.transfer_return_app = None;
                                return Err(anyhow!("Unexpected REFER response: {}", status));
                            }
                        }
                    }
                    Ok(None) => {
                        warn!(session_id = %self.id, "REFER timed out, no response received");
                        self.emit_refer_event(
                            408,
                            Some("timeout".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        self.meta.transfer_return_app = None;
                        return Err(anyhow!("REFER timed out"));
                    }
                    Err(e) => {
                        warn!(session_id = %self.id, error = %e, "Failed to send REFER");
                        self.emit_refer_event(
                            500,
                            Some(e.to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        self.meta.transfer_return_app = None;
                        return Err(anyhow!("Failed to send REFER: {}", e));
                    }
                }

                info!(session_id = %self.id,
                    "Blind transfer initiated — call will be transferred to {}",
                    uri
                );
                Ok(())
            }
        }
    }

    /// Dial the blind-transfer target as a new B leg in-session (B2BUA style).
    ///
    /// Used directly when `blind_transfer_use_refer` is disabled (or the
    /// disposition requires an anchored result), and as the 3PCC fallback
    /// when the peer rejects an outbound REFER with 405/420/501.
    async fn dial_blind_transfer_b2bua(
        &mut self,
        leg_id: LegId,
        uri: &str,
        refer_to_uri: &rsipstack::sip::Uri,
        from_user: Option<String>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        info!(session_id = %self.id, %leg_id, target = %uri, return_app = ?self.meta.transfer_return_app, "Blind transfer via B-leg INVITE (B2BUA)");
        // The transfer target is a NEW peer — invalidate the cached
        // callee offer so `prepare_callee_media_offer` creates a
        // fresh B leg (with its own local offer) instead of reusing
        // the previous callee's offer/leg, which would break
        // apply_sdp(answer) for the transferred-to endpoint.
        self.media.callee_offer = None;
        self.media.callee_offer_cached_webrtc = None;
        let caller = from_user
            .map(|user| format!("sip:{}@{}", user, refer_to_uri.host_with_port).parse())
            .transpose()
            .map_err(|e| anyhow!("Invalid transfer caller URI: {}", e))?;
        let mut location = crate::call::Location {
            aor: refer_to_uri.clone(),
            ..Default::default()
        };
        let mut registered = false;
        match self.server.locator.lookup(refer_to_uri).await {
            Ok(registered_locations) => {
                if let Some(registered_location) = registered_locations.into_iter().next() {
                    info!(
                        target = %refer_to_uri,
                        registered_contact = %registered_location.aor,
                        webrtc = registered_location.supports_webrtc,
                        transport = ?registered_location.transport,
                        "Resolved B-leg transfer target through locator"
                    );
                    location = registered_location;
                    registered = true;
                }
            }
            Err(error) => {
                warn!(
                    target = %refer_to_uri,
                    %error,
                    "Failed to resolve B-leg transfer target through locator; using bare SIP target"
                );
            }
        }
        // Not a registered internal contact — run the transfer target
        // through the route table (match/rewrite/trunk) if enabled.
        if !registered {
            match self.route_originated_leg(&location).await {
                Ok((routed, hints)) => {
                    location = routed;
                    self.track_routed_leg_hints(hints);
                }
                Err(e) => {
                    warn!(session_id = %self.id, %leg_id, target = %uri, error = %e, "Route lookup failed for transfer target; dialing directly");
                }
            }
        }
        let result = self
            .try_single_target(&location, callee_state_rx, None, None, caller)
            .await;
        if result.is_ok() {
            // The B2BUA blind-transfer path swaps the B leg
            // in-session (no REFER), so the REFER-based emitters
            // never fire. Emit the transfer notification here,
            // aligned with the inbound-REFER path's payload.
            self.emit_typed_rwi_event(&crate::rwi::CallTransferred {
                call_id: self.context.session_id.to_string(),
                transfer_target: Some(uri.to_string()),
            });
        }
        result.map_err(|(code, text, reason)| {
            self.meta.transfer_return_app = None;
            anyhow!(
                "B-leg transfer failed: {} {} - {}",
                code,
                text,
                reason.unwrap_or_default()
            )
        })
    }

    pub(crate) async fn handle_queue_transfer(
        &mut self,
        queue_name: &str,
        return_app: Option<ReturnTargetSpec>,
        target_overrides: Vec<String>,
    ) -> Result<()> {
        let queue_config = self
            .server
            .data_context
            .resolve_queue_config(queue_name)
            .await
            .map_err(|e| anyhow!("Failed to resolve queue config: {}", e))?;

        let queue_config = match queue_config {
            Some(config) => config,
            None => {
                // Queue configuration could not be resolved (e.g. a DB-backed
                // queue id that is not loaded). Returning a bare error here
                // leaves the caller in dead air — the session loop only logs it
                // as a WARN and the IVR app has already exited. Apply a graceful
                // fallback instead: record a trace event, then either return to
                // the app named by `return_app`, or play the service-unavailable
                // announcement and hang up (mirroring the queue app's own
                // fallback).
                return self
                    .handle_queue_failure_fallback(
                        queue_name,
                        "not found",
                        "queue.not_found",
                        return_app.as_ref(),
                    )
                    .await;
            }
        };

        let mut queue_plan = queue_config
            .to_queue_plan()
            .map_err(|e| anyhow!("Invalid queue config: {}", e))?;

        // The resolved config may not carry an internal `name` (e.g. a
        // skill-group queue synthesized on the fly). The reference IS the
        // queue identity in that case — backfill it so QueueApp writes
        // `meta.queue_name`, CDR `queue_id` and post-call hooks (CSAT) all
        // see the real queue instead of an empty string.
        if queue_plan.queue_name.is_empty() {
            queue_plan.queue_name = queue_name.to_string();
        }

        if !target_overrides.is_empty() {
            use crate::call::{DialStrategy, Location};
            let mut locations = Vec::new();
            for target in &target_overrides {
                let trimmed = target.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let location = if trimmed.starts_with("skillgroup:") {
                    let id = trimmed
                        .strip_prefix("skillgroup:")
                        .unwrap_or(trimmed)
                        .trim();
                    Location {
                        aor: rsipstack::sip::Uri::try_from(format!("skill-group:{}", id))
                            .map_err(|e| anyhow!("invalid target '{}': {}", trimmed, e))?,
                        contact_raw: Some(trimmed.to_string()),
                        ..Default::default()
                    }
                } else {
                    let uri = rsipstack::sip::Uri::try_from(trimmed)
                        .map_err(|e| anyhow!("invalid target '{}': {}", trimmed, e))?;
                    Location {
                        aor: uri.clone(),
                        contact_raw: Some(uri.to_string()),
                        ..Default::default()
                    }
                };
                locations.push(location);
            }
            if !locations.is_empty() {
                info!(session_id = %self.id,
                    overrides = %locations.len(),
                    "Queue transfer: overriding targets from query params"
                );
                queue_plan.dial_strategy = Some(DialStrategy::Sequential(locations));
            }
        }

        // If return_app is set, override the fallback so that on queue
        // failure the caller is transferred back to the return app.
        if let Some(spec) = &return_app {
            let fallback_endpoint = match spec.app_name.as_str() {
                "ivr" => {
                    let ivr_name = spec.target.as_deref().unwrap_or("");
                    info!(session_id = %self.id,
                        queue = %queue_name,
                        ivr = %ivr_name,
                        "Queue transfer: will return to IVR on fallback"
                    );
                    crate::call::TransferEndpoint::Ivr(ivr_name.to_string())
                }
                "voicemail" => {
                    let ext = spec.target.as_deref().unwrap_or("");
                    crate::call::TransferEndpoint::Voicemail(ext.to_string())
                }
                "queue" => {
                    let name = spec.target.as_deref().unwrap_or("");
                    crate::call::TransferEndpoint::Queue(name.to_string())
                }
                "conference" => {
                    let id = spec.target.as_deref().unwrap_or("");
                    crate::call::TransferEndpoint::Conference(id.to_string())
                }
                _ => {
                    // Generic app — use IVR as the fallback endpoint with the
                    // app name so it re-enters routing.  This is a best-effort
                    // path for non-standard apps.
                    let target = spec.target.as_deref().unwrap_or(&spec.app_name);
                    crate::call::TransferEndpoint::Ivr(target.to_string())
                }
            };
            queue_plan.fallback = Some(crate::call::QueueFallbackAction::Failure(
                crate::call::FailureAction::Transfer(fallback_endpoint),
            ));
        }

        if let Err(e) = self.start_queue_app(queue_plan).await {
            warn!(session_id = %self.id, queue = %queue_name, error = %e, "Queue app failed to start; applying graceful fallback");
            return self
                .handle_queue_failure_fallback(
                    queue_name,
                    &format!("start failed ({})", e),
                    "queue.start_failed",
                    return_app.as_ref(),
                )
                .await;
        }

        // Store the resolved return app on meta so that when the connected
        // agent (B‑leg) hangs up, the session returns the caller to the app
        // instead of tearing down the call.
        self.meta.transfer_return_app = self.resolve_return_app(return_app).await;
        info!(session_id = %self.id, queue = %queue_name, return_app = ?self.meta.transfer_return_app, "Queue transfer completed: queue app started");
        Ok(())
    }

    /// Graceful fallback when a queue cannot be serviced: record an error
    /// trace, then either return to the app named by `return_app` (if set) or
    /// play the service-unavailable announcement and hang up. Shared by the
    /// queue-not-found and queue-app-start-failure paths so the caller is never
    /// left in dead air after the IVR has already handed the call over.
    async fn handle_queue_failure_fallback(
        &mut self,
        queue_name: &str,
        reason: &str,
        code: &str,
        return_app: Option<&ReturnTargetSpec>,
    ) -> Result<()> {
        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Queue,
                format!("Queue '{}' {} — using fallback", queue_name, reason),
            )
            .severity(crate::call_errors::ErrSeverity::Error)
            .code(code),
        );

        if let Some(spec) = return_app {
            info!(
                session_id = %self.id,
                queue = %queue_name,
                app = %spec.app_name,
                target = ?spec.target,
                "Queue failed; returning to app"
            );
            let resolved = self.resolve_return_app(Some(spec.clone())).await;
            if let Some(rspec) = resolved {
                return self
                    .ensure_app_running(
                        &rspec.app_name,
                        Some(rspec.params),
                        &format!("Return app '{}' (queue failed)", rspec.app_name),
                    )
                    .await;
            }
        }

        warn!(
            session_id = %self.id,
            queue = %queue_name,
            "Queue failed; playing service-unavailable announcement and hanging up"
        );
        let fallback_plan = crate::call::QueuePlan {
            queue_name: queue_name.to_string(),
            voice_prompts: Some(crate::call::VoicePrompts {
                busy_prompt: Some(crate::call::DEFAULT_QUEUE_FAILURE_AUDIO.to_string()),
                ..crate::call::VoicePrompts::default()
            }),
            ..crate::call::QueuePlan::default()
        };
        self.start_queue_app(fallback_plan).await
    }

    async fn start_route_point_app(
        &mut self,
        route_point: &str,
        variables: HashMap<String, String>,
    ) -> Result<()> {
        let caller = self
            .context
            .dialplan
            .caller
            .clone()
            .ok_or_else(|| anyhow!("route-point transfer has no caller identity"))?;
        let contact = self
            .context
            .dialplan
            .caller_contact
            .as_ref()
            .map(|contact| contact.uri.clone())
            .unwrap_or_else(|| caller.clone());
        let realm = self.server.proxy_config.load().select_realm("");
        let target = crate::call::build_sip_uri(route_point, &realm);
        let target_uri = rsipstack::sip::Uri::try_from(target.as_str())
            .map_err(|error| anyhow!("invalid route-point target: {error}"))?;
        let current_invocation = self.app_runtime.current_app_invocation().await;
        let current_headers = current_invocation
            .as_ref()
            .map(|context| context.sip_headers.clone())
            .unwrap_or_else(|| {
                self.app_runtime
                    .app_context()
                    .map(|context| context.call_info.sip_headers.clone())
                    .unwrap_or_default()
            });
        let carry_headers = current_headers
            .iter()
            .map(|(name, value)| rsipstack::sip::Header::Other(name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let routed = super::util::route_leg(
            &self.server,
            &target_uri,
            &caller,
            &contact,
            (!carry_headers.is_empty()).then_some(carry_headers),
            &self.context.dialplan.direction,
            self.context.cookie.clone(),
        )
        .await?;

        match routed {
            Some(crate::config::RouteResult::Application {
                option,
                app_name,
                app_params,
                auto_answer,
                hints,
            }) => {
                self.track_routed_leg_hints(hints);
                let sip_headers = crate::call::app::merge_sip_headers(
                    &current_headers,
                    option.headers.as_deref().unwrap_or_default(),
                );
                let route_context = crate::call::app::AppRouteContext {
                    callee: route_point.to_string(),
                    sip_headers,
                    variables: variables.clone(),
                };
                if let Err(error) = self
                    .ensure_app_running_with_route_context(
                        &app_name,
                        app_params,
                        auto_answer,
                        &format!("route-point application '{app_name}'"),
                        route_context,
                    )
                    .await
                {
                    return self
                        .try_route_point_fallback_or_terminate(
                            error,
                            &format!("toivr:{route_point}"),
                            &variables,
                        )
                        .await;
                }
                Ok(())
            }
            Some(crate::config::RouteResult::Abort(code, reason)) => {
                let status = code.code();
                self.terminate_route_point_handoff(
                    anyhow!(
                        "route aborted for IVR route point: {} {}",
                        status,
                        reason.unwrap_or_default()
                    ),
                    super::util::sip_status_to_hangup_reason(status),
                    Some(status),
                )
                .await
            }
            Some(crate::config::RouteResult::Forward(_, hints))
            | Some(crate::config::RouteResult::NotHandled(_, hints)) => {
                self.track_routed_leg_hints(hints);
                self.try_route_point_fallback_or_terminate(
                    anyhow!("route point did not resolve to an application"),
                    &format!("toivr:{route_point}"),
                    &variables,
                )
                .await
            }
            Some(crate::config::RouteResult::Queue { hints, .. }) => {
                self.track_routed_leg_hints(hints);
                self.try_route_point_fallback_or_terminate(
                    anyhow!("route point resolved to an unsupported queue"),
                    &format!("toivr:{route_point}"),
                    &variables,
                )
                .await
            }
            None => {
                self.try_route_point_fallback_or_terminate(
                    anyhow!("route point was not handled"),
                    &format!("toivr:{route_point}"),
                    &variables,
                )
                .await
            }
        }
    }

    async fn try_route_point_fallback_or_terminate(
        &mut self,
        error: anyhow::Error,
        route_point: &str,
        variables: &HashMap<String, String>,
    ) -> Result<()> {
        match self
            .try_ivr_fallback_after_start_failure(error, route_point, variables)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                let failure = crate::call::app::error_catalog::IVR_START_FAILED;
                self.terminate_route_point_handoff(
                    error,
                    failure.hangup_reason.clone(),
                    failure.sip_status,
                )
                .await
            }
        }
    }

    async fn terminate_route_point_handoff(
        &mut self,
        error: anyhow::Error,
        reason: crate::callrecord::CallRecordHangupReason,
        code: Option<u16>,
    ) -> Result<()> {
        let hangup = self
            .handle_hangup(&crate::call::domain::HangupCommand::all(Some(reason), code))
            .await;
        if !hangup.success {
            return Err(anyhow!(
                "{}; terminal hangup failed: {}",
                error,
                hangup.message.unwrap_or_default()
            ));
        }
        Err(error)
    }

    /// Resolve a raw [`ReturnTargetSpec`] (extracted from query params) into a
    /// concrete [`ReturnAppSpec`] ready to be stored on `CallMeta`.
    ///
    /// For `"ivr"` apps the `target` field is resolved through
    /// `data_context.resolve_ivr_file`.  For all other apps the params
    /// HashMap is serialised to JSON as-is.
    async fn resolve_return_app(&self, raw: Option<ReturnTargetSpec>) -> Option<ReturnAppSpec> {
        let spec = raw?;
        match spec.app_name.as_str() {
            "ivr" => {
                let ivr_name = spec.target.as_deref().unwrap_or("default");
                let ivr_file = self.server.data_context.resolve_ivr_file(ivr_name).await;
                Some(ReturnAppSpec::ivr(ivr_file, spec.params))
            }
            _ => {
                let mut params = serde_json::Map::new();
                if let Some(t) = spec.target {
                    params.insert("target".into(), serde_json::Value::String(t));
                }
                for (k, v) in spec.params {
                    params.insert(k, serde_json::Value::String(v));
                }
                Some(ReturnAppSpec {
                    app_name: spec.app_name,
                    params: serde_json::Value::Object(params),
                })
            }
        }
    }

    pub(crate) async fn start_ivr_app(
        &self,
        ivr_name: &str,
        query_params: HashMap<String, String>,
    ) -> Result<()> {
        let ivr_file = self.server.data_context.resolve_ivr_file(ivr_name).await;
        info!(session_id = %self.id, ivr = %ivr_name, file = %ivr_file, "Starting IVR application");
        // Remember the IVR short code so a later queue dispatch can inject
        // `User-to-User` `ivr=` (desk_rustpbx.md §3.2).
        self.session_ext_set("ivr", ivr_name);
        let mut app_params = serde_json::json!({"file": ivr_file});
        if !query_params.is_empty() {
            app_params["ivr_params"] = serde_json::json!(query_params);
        }
        match self
            .ensure_app_running("ivr", Some(app_params), &format!("IVR '{}'", ivr_name))
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                self.try_ivr_fallback_after_start_failure(e, ivr_name, &query_params)
                    .await
            }
        }
    }

    /// When starting a named IVR fails, try once more with `[proxy.ivr_fallback]`.
    pub(crate) async fn try_ivr_fallback_after_start_failure(
        &self,
        original: anyhow::Error,
        failed_ivr: &str,
        query_params: &HashMap<String, String>,
    ) -> Result<()> {
        use crate::call::app::ivr::fallback::{self, IVR_FALLBACK_USED_KEY};

        let already_used = query_params
            .get(IVR_FALLBACK_USED_KEY)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
            || self
                .app_runtime
                .app_context()
                .and_then(|ctx| {
                    ctx.session_vars
                        .get(IVR_FALLBACK_USED_KEY)
                        .map(|e| e.value().clone())
                })
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        if already_used {
            return Err(original);
        }

        let proxy_cfg = self.server.proxy_config.load();
        let Some(fb) = proxy_cfg
            .ivr_fallback
            .as_ref()
            .filter(|c| c.is_configured())
        else {
            return Err(original);
        };

        let invocation = self.app_runtime.current_app_invocation().await;
        let call_info = self
            .app_runtime
            .app_context()
            .map(|context| context.call_info.clone());
        let caller = call_info
            .as_ref()
            .map(|info| info.caller.clone())
            .unwrap_or_default();
        let callee = invocation
            .as_ref()
            .map(|context| context.callee.clone())
            .or_else(|| call_info.as_ref().map(|info| info.callee.clone()))
            .unwrap_or_default();
        let headers = invocation
            .map(|context| context.sip_headers)
            .or_else(|| call_info.map(|info| info.sip_headers));

        let Some(target) =
            fallback::resolve_fallback_target(fb, &caller, &callee, headers.as_ref())
        else {
            return Err(original);
        };

        if target == failed_ivr {
            return Err(original);
        }

        warn!(
            session_id = %self.id,
            error = %original,
            target = %target,
            "IVR start failed, retrying with ivr_fallback target"
        );

        if let Some(ctx) = self.app_runtime.app_context() {
            ctx.session_vars
                .insert(IVR_FALLBACK_USED_KEY.into(), "1".into());
        }

        let mut qp = query_params.clone();
        qp.insert(IVR_FALLBACK_USED_KEY.into(), "1".into());
        let ivr_file = self.server.data_context.resolve_ivr_file(&target).await;
        let mut app_params = serde_json::json!({"file": ivr_file});
        app_params["ivr_params"] = serde_json::json!(qp);
        self.ensure_app_running(
            "ivr",
            Some(app_params),
            &format!("IVR fallback '{}'", target),
        )
        .await
    }

    pub(crate) async fn start_voicemail_app(&mut self, extension: &str) -> Result<()> {
        info!(session_id = %self.id, extension = %extension, "Starting voicemail application");
        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Voicemail,
                format!("Voicemail: caller routed to mailbox '{}'", extension),
            )
            .severity(crate::call_errors::ErrSeverity::Info),
        );
        let params = Some(serde_json::json!({"extension": extension}));
        self.ensure_app_running(
            "voicemail",
            params,
            &format!("voicemail for '{}'", extension),
        )
        .await
    }

    /// Start a conference app that joins the session into the given conference room.
    pub(crate) async fn start_conference_app(&self, conf_id: &str) -> Result<()> {
        info!(session_id = %self.id, conf_id = %conf_id, "Starting conference application");
        let params = Some(serde_json::json!({"id": conf_id}));
        self.ensure_app_running("conference", params, &format!("conference '{}'", conf_id))
            .await
    }

    /// Establish a WebSocket + PCM real‑time bridge to an external VoIP endpoint.
    ///
    /// Full‑duplex audio bridge between the SIP call leg and a WebSocket carrying
    /// raw PCM16 (i16 little‑endian, no framing header).
    ///
    /// ┌──────────────────────────────────────────────────────────────────┐
    /// │  SipSession                                                     │
    /// │  ┌──── forward_loop ─────────────────────────────────────────┐  │
    /// │  │ WS raw PCM16 → buffer → resample → encode → audio_sender  │  │
    /// │  └────────────────────────────────────────────────────────────┘  │
    /// │  ┌──── reverse_loop ─────────────────────────────────────────┐  │
    /// │  │ audio_receiver → resample → raw PCM16 → WS send           │  │
    /// │  └────────────────────────────────────────────────────────────┘  │
    /// └──────────────────────────────────────────────────────────────────┘
    pub(crate) async fn connect_bridge(
        &mut self,
        leg_id: LegId,
        endpoint: String,
        _headers: HashMap<String, String>,
        sample_rate: u32,
        codec: String,
        timeout_ms: Option<u64>,
        return_app: Option<ReturnTargetSpec>,
        trace_context: Option<BridgeTraceContext>,
    ) -> Result<()> {
        info!(session_id = %self.id, %leg_id, endpoint = %endpoint, sample_rate, codec = %codec, "Connecting Bridge");

        // Arm the DTMF trace context BEFORE the media loops start so digits
        // arriving during the bridge are reported against the originating
        // IVR node (and buffered for the return-app flow).
        *self.bridge_trace_context.lock() = trace_context;
        self.bridge_dtmf_digits.lock().clear();

        // Captured for the spawn'd forward/reverse loops (self is moved).
        let session_id = self.id.to_string();

        // ── 1. Establish WebSocket connection ──────────────────────────
        let ws_connect = tokio_tungstenite::connect_async(&endpoint);
        let (ws_stream, _) = if let Some(ms) = timeout_ms {
            tokio::time::timeout(Duration::from_millis(ms), ws_connect)
                .await
                .map_err(|_| anyhow!("Bridge connection timed out after {}ms", ms))?
                .map_err(|e| anyhow!("Failed to connect Bridge WebSocket: {}", e))?
        } else {
            ws_connect
                .await
                .map_err(|e| anyhow!("Failed to connect Bridge WebSocket: {}", e))?
        };
        info!(session_id = %self.id, endpoint = %endpoint, "Bridge WebSocket connected");
        let (mut ws_write, mut ws_read) = ws_stream.split();

        // ── 2. Obtain the leg's audio sender (forward) & PeerConnection (reverse).
        let mut forward_sink: Option<BridgeForwardSink> = None;
        let mut pc: Option<PeerConnection> = None;

        // Fallback: leg's VoiceEnginePeer tracks (non-app B2BUA path).
        if forward_sink.is_none() || pc.is_none() {
            let peer = self
                .legs
                .get_peer(&leg_id)
                .cloned()
                .or_else(|| self.caller_peer().cloned())
                .ok_or_else(|| anyhow!("No media peer available"))?;

            let tracks = peer.get_tracks().await;
            for t in &tracks {
                if forward_sink.is_none() {
                    if let Some(sender) = t.get_sender() {
                        forward_sink = Some(BridgeForwardSink::Track(sender));
                    }
                }
                if pc.is_none() {
                    pc = t.get_peer_connection().await;
                }
            }
        }

        // App-anchored flow (IVR / queue / voicemail): caller media lives on
        // the MediaBridge A leg, not on VoiceEnginePeer tracks. Use a raw-PCM
        // channel source — the leg's egress pipeline encodes to the negotiated
        // codec (same "filetrack" mode as play_file).
        let ws_sample_rate = if sample_rate == 0 { 8000 } else { sample_rate };
        let mut pcm_ended_rx: Option<tokio::sync::oneshot::Receiver<()>> = None;
        if forward_sink.is_none() || pc.is_none() {
            if let Some(mb) = self.media.bridge.as_ref()
                && let Some(leg) = mb.leg(crate::media::media_bridge::LegSide::A)
            {
                info!(session_id = %self.id, %leg_id, rate = ws_sample_rate,
                    "Bridge sourcing caller media from MediaBridge A leg (raw PCM channel)");
                if forward_sink.is_none() {
                    let (end_tx, end_rx) = tokio::sync::oneshot::channel();
                    let end_tx = std::sync::Mutex::new(Some(end_tx));
                    let on_end: crate::media::egress::EgressEndCallback =
                        std::sync::Arc::new(move |_interrupted| {
                            if let Ok(mut slot) = end_tx.lock() {
                                if let Some(tx) = slot.take() {
                                    let _ = tx.send(());
                                }
                            }
                        });
                    match mb
                        .bridge_play_pcm(
                            crate::media::media_bridge::LegSide::A,
                            ws_sample_rate,
                            Some(on_end),
                        )
                        .await
                    {
                        Ok(tx) => {
                            forward_sink = Some(BridgeForwardSink::Pcm(tx));
                            pcm_ended_rx = Some(end_rx);
                        }
                        Err(e) => warn!(session_id = %self.id, %leg_id, error = %e,
                            "Failed to set up raw PCM channel for bridge forward"),
                    }
                }
                if pc.is_none() {
                    pc = Some(leg.pc().clone());
                }
            }
        }

        let forward_sink = forward_sink.ok_or_else(|| anyhow!("No forward sink for Bridge"))?;
        let pc = pc.ok_or_else(|| anyhow!("No PeerConnection for Bridge"))?;

        // MediaBridge legs are authoritative for app-anchored calls because
        // apply_sdp() stores the actual negotiated codec/PT/DTMF profile there.
        // Keep the session SDP caches as a fallback for legacy peer paths.
        let negotiated_profile = self
            .media_side_for_leg(&leg_id)
            .and_then(|side| {
                self.media
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.leg(side))
                    .and_then(|leg| leg.negotiated())
            })
            .or_else(|| {
                self.legs
                    .get_answer(&leg_id)
                    .or_else(|| {
                        if leg_id.as_str() == "caller" {
                            self.media.answer.as_deref()
                        } else if leg_id.as_str() == "callee" {
                            self.media.callee_answer_sdp.as_deref()
                        } else {
                            None
                        }
                    })
                    .map(MediaNegotiator::extract_leg_profile)
            });

        // ── 3. Determine codec type + payload type ───────────────────────
        // Prefer the selected leg's negotiated codec and PT so the injected
        // audio and reverse decoder use the profile that leg actually accepted.
        // The bridge URL `codec` query param is only a fallback.
        //
        // The forward loop used to encode the `codec` param and tag frames with
        // `codec_type.payload_type()` — the codec's STATIC default PT (Opus=111,
        // PCMU=0). When that differs from the caller's negotiated PT (e.g. Opus
        // negotiated at 96, or a PCMU caller bridged with codec=opus), the
        // forward frames carry a PT the caller never offered, which on the same
        // SSRC as the IVR greeting shows up as a PT 0↔96/111 toggle.
        let (codec_type, payload_type) = if let Some(audio) = negotiated_profile
            .as_ref()
            .and_then(|profile| profile.audio.as_ref())
        {
            info!(
                %leg_id,
                negotiated_codec = ?audio.codec,
                negotiated_pt = audio.payload_type,
                negotiated_clock_rate = audio.clock_rate,
                bridge_codec = %codec,
                "voip_bridge using leg-negotiated codec/PT"
            );
            (audio.codec, audio.payload_type)
        } else {
            let fallback = match codec.as_str() {
                "pcm" | "pcmu" => audio_codec::CodecType::PCMU,
                "pcma" | "g711" => audio_codec::CodecType::PCMA,
                "opus" => audio_codec::CodecType::Opus,
                "g722" => audio_codec::CodecType::G722,
                _ => self.leg_negotiated_codec(&leg_id),
            };
            (fallback, fallback.payload_type())
        };

        // Decode the reverse RTP stream with the same codec selected for this
        // leg. Using the session-level decoder here defaulted RWI calls to PCMU
        // even after the real MediaBridge leg had negotiated Opus.
        let mut decoder = audio_codec::create_decoder(codec_type);
        let dec_sample_rate = decoder.sample_rate();
        let ws_sample_rate = if sample_rate == 0 { 8000 } else { sample_rate };

        // ── 5. Cancellation token (parent = session cancel) ──────────
        let cancel_token = self.cancel_token.child_token();

        // ── 5b. DTMF payload types (from answer SDP) ─────────────────
        let mut dtmf_payload_types: Vec<u8> = negotiated_profile
            .as_ref()
            .map(|profile| profile.dtmf_pts().into_iter().collect())
            .unwrap_or_default();
        dtmf_payload_types.sort_unstable();
        dtmf_payload_types.dedup();

        // ── 5c. DTMF JSON text-frame channel ─────────────────────────
        let (dtmf_json_tx, mut dtmf_json_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        *self.bridge_dtmf_tx.write() = Some(dtmf_json_tx.clone());
        let bridge_dtmf_tx_state = self.bridge_dtmf_tx.clone();

        let cmd_tx_for_fwd = self.cmd_tx.clone();

        // ── 6. Forward loop: WS raw PCM16 → call ─────────────────────
        // Two paths:
        //   Track (B2BUA): encode → push pre-encoded AudioFrame to PC track
        //   Pcm   (app):   send raw PCM16 chunks to the leg's egress pipeline,
        //                   which encodes to the negotiated codec (filetrack mode)
        let forward_cancel = cancel_token.child_token();
        let forward_handle = {
            let leg_id = leg_id.clone();
            let session_id = session_id.clone();
            crate::utils::spawn(async move {
                use audio_codec::create_encoder;
                use rustrtc::media::{AudioFrame as RtcAudioFrame, MediaSample};

                let samples_per_frame = (ws_sample_rate * 20 / 1000) as usize;
                let mut buf: Vec<i16> = Vec::new();

                // Track path: encoder + RTP state. Pcm path: none needed.
                let mut encoder = if let BridgeForwardSink::Track(..) = &forward_sink {
                    Some(create_encoder(codec_type))
                } else {
                    None
                };
                let enc_sample_rate = encoder
                    .as_ref()
                    .map(|e| e.sample_rate())
                    .unwrap_or(ws_sample_rate);
                let clock_rate = codec_type.clock_rate() as u32;
                let rtp_ticks_per_frame = clock_rate * 20 / 1000;
                let mut rtp_ts: u32 = rand::random();
                let mut seq: u16 = rand::random();

                loop {
                    tokio::select! {
                        biased;
                        _ = forward_cancel.cancelled() => {
                            info!(session_id = %session_id, %leg_id, "Bridge forward loop cancelled");
                            break;
                        }
                        msg = ws_read.next() => {
                            match msg {
                                Some(Ok(Message::Binary(data))) => {
                                    if data.len() < 2 { continue; }
                                    let samples: Vec<i16> = data.chunks_exact(2)
                                        .map(|c| i16::from_ne_bytes([c[0], c[1]]))
                                        .collect();
                                    buf.extend(samples);

                                    while buf.len() >= samples_per_frame {
                                        let chunk: Vec<i16> = buf.drain(..samples_per_frame).collect();

                                        if let BridgeForwardSink::Pcm(tx) = &forward_sink {
                                            tokio::select! {
                                                biased;
                                                _ = forward_cancel.cancelled() => return,
                                                result = tx.send(chunk) => {
                                                    if result.is_err() {
                                                        info!(%session_id, %leg_id, "Bridge forward: PCM channel closed");
                                                        return;
                                                    }
                                                }
                                            }
                                        } else if let BridgeForwardSink::Track(sender) = &forward_sink {
                                            let chunk = if ws_sample_rate != enc_sample_rate {
                                                crate::call::runtime::conference_media_bridge::resample_linear(
                                                    &chunk, ws_sample_rate, enc_sample_rate,
                                                )
                                            } else {
                                                chunk
                                            };
                                            if let Some(ref mut enc) = encoder {
                                                let encoded = enc.encode(&chunk);
                                                let frame = RtcAudioFrame {
                                                    rtp_timestamp: rtp_ts,
                                                    clock_rate,
                                                    data: encoded.into(),
                                                    sequence_number: Some(seq),
                                                    payload_type: Some(payload_type),
                                                    marker: false,
                                                    header_extension: None,
                                                    raw_packet: None,
                                                    source_addr: None,
                                                };
                                                if sender.send(MediaSample::Audio(frame)).is_err() {
                                                    warn!(%session_id, %leg_id, "Bridge forward: track sender closed");
                                                    return;
                                                }
                                                rtp_ts = rtp_ts.wrapping_add(rtp_ticks_per_frame);
                                                seq = seq.wrapping_add(1);
                                            }
                                        }
                                    }
                                }
                                Some(Ok(Message::Text(txt))) => {
                                    // Outbound DTMF: WS service sends {"type":"dtmf","digit":"..."}
                                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&txt) {
                                        if val.get("type").and_then(|v| v.as_str()) == Some("dtmf") {
                                            if let Some(digits) = val.get("digit").and_then(|v| v.as_str()) {
                                                if let Some(ref tx) = cmd_tx_for_fwd {
                                                    let cmd = CallCommand::SendDtmf {
                                                        leg_id: leg_id.clone(),
                                                        digits: digits.to_string(),
                                                    };
                                                    tokio::select! {
                                                        biased;
                                                        _ = forward_cancel.cancelled() => return,
                                                        result = tx.send(cmd) => {
                                                            if result.is_err() {
                                                                warn!(session_id = %session_id, %leg_id, "Bridge forward: cmd_tx closed");
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    info!(session_id = %session_id, %leg_id, "Bridge WS closed remotely");
                                    break;
                                }
                                Some(Err(e)) => {
                                    warn!(session_id = %session_id, %leg_id, "Bridge WS read error: {}", e);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            })
        };

        // ── 7. Reverse loop: call audio → raw PCM16 → WS + DTMF JSON ─
        //     DTMF JSON comes from the session-level deduplicated event channel.
        let reverse_cancel = cancel_token.child_token();
        let reverse_handle = {
            let leg_id = leg_id.clone();
            let session_id = session_id.clone();
            crate::utils::spawn(async move {
                use rustrtc::media::MediaSample;

                // Capture audio track from PeerConnection
                let track = loop {
                    if let Some(t) = SipSession::find_audio_receiver_track(&pc).await {
                        break t;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if reverse_cancel.is_cancelled() {
                        return;
                    }
                };

                loop {
                    tokio::select! {
                        biased;
                        _ = reverse_cancel.cancelled() => {
                            info!(session_id = %session_id, %leg_id, "Bridge reverse loop cancelled");
                            break;
                        }
                        json = dtmf_json_rx.recv() => {
                            match json {
                                Some(json) => {
                                    tokio::select! {
                                        biased;
                                        _ = reverse_cancel.cancelled() => break,
                                        result = ws_write.send(Message::Text(json.into())) => {
                                            if result.is_err() {
                                                warn!(session_id = %session_id, %leg_id, "Bridge WS DTMF json write failed");
                                                break;
                                            }
                                        }
                                    }
                                }
                                None => break,
                            }
                        }
                        sample = track.recv() => {
                            match sample {
                                Ok(MediaSample::Audio(frame)) => {
                                    let is_dtmf = frame.payload_type
                                        .map_or(false, |pt| dtmf_payload_types.contains(&pt));

                                    if !is_dtmf {
                                        // Regular audio frame — decode to PCM, resample, send as binary
                                        let pcm = decoder.decode(&frame.data);
                                        let samples = if dec_sample_rate != ws_sample_rate {
                                            crate::call::runtime::conference_media_bridge::resample_linear(
                                                &pcm, dec_sample_rate, ws_sample_rate,
                                            )
                                        } else {
                                            pcm
                                        };
                                        let mut bytes = Vec::with_capacity(samples.len() * 2);
                                        for s in &samples {
                                            bytes.extend_from_slice(&s.to_ne_bytes());
                                        }
                                        tokio::select! {
                                            biased;
                                            _ = reverse_cancel.cancelled() => break,
                                            result = ws_write.send(Message::Binary(bytes.into())) => {
                                                if result.is_err() {
                                                    warn!(session_id = %session_id, %leg_id, "Bridge reverse audio write failed");
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(session_id = %session_id, %leg_id, "Bridge reverse track error: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }

                // Cleanup: clear bridge_dtmf_tx so SIP INFO no longer tries to forward
                *bridge_dtmf_tx_state.write() = None;
            })
        };

        // ── 8. Store bridge reference on session ─────────────────────
        self.conference_bridge = crate::call::runtime::SessionConferenceBridge {
            bridge_handle: Some(crate::call::runtime::ConferenceBridgeHandle {
                _tasks: vec![],
                cancel_token: cancel_token.clone(),
            }),
            conf_id: Some(format!("bridge-{}", self.id.0)),
        };

        // ── 9. Write return app to CallMeta + spawn disconnect monitor ──
        //    The monitor sends `StartReturnApp` on bridge disconnect; the
        //    handler reads `meta.transfer_return_app` (written here).
        let has_return_app = return_app.is_some();
        self.meta.transfer_return_app = self.resolve_return_app(return_app).await;
        let cancel = self.cancel_token.child_token();
        let tx = self.cmd_tx.clone();
        let mon_session_id = session_id.clone();
        let mon = crate::utils::spawn(async move {
            let bridge_disconnected = wait_for_bridge_disconnect(
                cancel.clone(),
                cancel_token,
                forward_handle,
                reverse_handle,
                pcm_ended_rx,
            )
            .await;
            if bridge_disconnected
                && has_return_app
                && let Some(tx) = tx
            {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {}
                    result = tx.send(CallCommand::StartReturnApp) => {
                        if result.is_ok() {
                            info!(session_id = %mon_session_id, "Bridge disconnected; starting return app");
                        }
                    }
                }
            }
        });
        self.legs.push_task(leg_id.clone(), mon);

        info!(session_id = %self.id, %leg_id, endpoint = %endpoint, "Bridge established");
        // A real media bridge is now active between caller and endpoint — the
        // transfer window is over, restore normal RTP watchdog monitoring.
        self.meta.transfer_in_progress = false;
        self.sync_rtp_timeout_pause();
        Ok(())
    }

    pub(super) fn build_replaces_header(&self) -> Option<String> {
        let dialog_id = self.caller_dialog_id();

        let call_id = &dialog_id.call_id;
        let local_tag = &dialog_id.local_tag;
        let remote_tag = &dialog_id.remote_tag;

        if remote_tag.is_empty() {
            return None;
        }

        Some(format!(
            "{};to-tag={};from-tag={}",
            call_id, local_tag, remote_tag
        ))
    }

    pub(super) async fn handle_replace_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        let replaces = self
            .build_replaces_header()
            .ok_or_else(|| anyhow!("Cannot build Replaces header for current dialog"))?;
        let encoded_replaces = urlencoding::encode(&replaces).into_owned();

        let refer_target = if target.contains('?') {
            format!("{}&Replaces={}", target, encoded_replaces)
        } else {
            format!("{}?Replaces={}", target, encoded_replaces)
        };

        self.handle_blind_transfer(
            leg_id,
            refer_target,
            TransferDisposition::Detach,
            callee_state_rx,
        )
        .await
    }

    pub(super) async fn emit_refer_event(
        &self,
        sip_status: u16,
        reason: Option<String>,
        event_type: crate::call::domain::ReferNotifyEventType,
    ) {
        let event = crate::call::domain::ReferNotifyEvent {
            call_id: self.id.0.clone(),
            sip_status,
            reason,
            event_type,
        };
        let subscribers = self.server.transfer_notify_subscribers.lock().await;
        for tx in subscribers.iter() {
            let _ = tx.send(event.clone());
        }
    }

    pub(super) async fn handle_transfer_complete(&mut self, consult_leg: LegId) -> Result<()> {
        info!(session_id = %self.id, %consult_leg, "Completing attended transfer");

        self.require_leg(&consult_leg)?;

        let original_leg = self
            .legs
            .iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            if self
                .setup_bridge(original_leg.clone(), consult_leg.clone())
                .await
            {
                self.update_leg_state(&original_leg, LegState::Connected);
                self.update_leg_state(&consult_leg, LegState::Connected);
                let _ = self.handle_unhold(original_leg.clone()).await;
                info!(session_id = %self.id, "Attended transfer completed successfully");
                self.record_trace(
                    crate::call_errors::TraceEvent::new(
                        crate::call_errors::TraceKind::Transfer,
                        "Attended transfer completed",
                    )
                    .severity(crate::call_errors::ErrSeverity::Info),
                );
                info!("Attended transfer completed successfully");
            } else {
                return Err(anyhow!("Failed to setup bridge for transfer completion"));
            }
        } else {
            return Err(anyhow!("No leg on hold found for transfer completion"));
        }

        Ok(())
    }

    pub(super) async fn handle_transfer_cancel(&mut self, consult_leg: LegId) -> Result<()> {
        info!(session_id = %self.id, %consult_leg, "Canceling attended transfer");

        self.require_leg(&consult_leg)?;
        self.update_leg_state(&consult_leg, LegState::Ending);

        let original_leg = self
            .legs
            .iter()
            .find(|(_, leg)| leg.state == LegState::Hold)
            .map(|(id, _)| id.clone());

        if let Some(original_leg) = original_leg {
            self.update_leg_state(&original_leg, LegState::Connected);
            let _ = self.handle_unhold(original_leg.clone()).await;
            info!(session_id = %self.id, "Attended transfer canceled, original call resumed");
        }

        Ok(())
    }

    pub(super) async fn handle_transfer_complete_cross_session(
        &mut self,
        from_session: String,
        leg_id: LegId,
        into_conference: String,
    ) -> Result<()> {
        info!(session_id = %self.id,
            from_session = %from_session,
            leg_id = %leg_id,
            into_conference = %into_conference,
            "Handling cross-session transfer completion"
        );

        if self.id.to_string() != from_session {
            self.forward_command(
                &from_session,
                CallCommand::TransferCompleteCrossSession {
                    from_session: from_session.clone(),
                    leg_id,
                    into_conference,
                },
                "forward cross-session transfer",
            )?;
            return Ok(());
        }

        let leg = self
            .legs
            .get(&leg_id)
            .ok_or_else(|| anyhow!("Leg {} not found in session {}", leg_id, from_session))?;

        info!(session_id = %self.id,
            session_id = %self.id,
            leg_id = %leg_id,
            leg_state = ?leg.state,
            "Found leg for cross-session migration"
        );

        let conference_server = &self.server.conference_server;
        let conf_id = crate::call::runtime::ConferenceId::from(into_conference.as_str());

        // Use a consistent composite leg_id for both conference registration and
        // media bridge start — previously the two used different IDs causing a mismatch.
        let participant_leg = LegId::new(format!("{}-{}", from_session, leg_id));
        conference_server
            .add_participant(&conf_id, participant_leg.clone())
            .await
            .map_err(|e| anyhow!("Failed to add leg to conference: {}", e))?;

        info!(session_id = %self.id,
            session_id = %self.id,
            leg_id = %leg_id,
            conf_id = %into_conference,
            "Successfully migrated leg into conference"
        );

        self.try_start_and_store_bridge(
            &into_conference,
            &participant_leg,
            "conference media bridge",
        )
        .await;

        self.update_leg_state(&leg_id, LegState::Hold);

        Ok(())
    }

    pub(super) async fn handle_bridge_cross_session(
        &mut self,
        session_a: String,
        leg_a: LegId,
        session_b: String,
        leg_b: LegId,
    ) -> Result<()> {
        let current_session = self.id.to_string();

        info!(session_id = %self.id,
            current_session = %current_session,
            session_a = %session_a,
            session_b = %session_b,
            "Handling cross-session P2P bridge"
        );

        let conf_id = if session_a < session_b {
            format!("p2p-bridge-{}-{}", session_a, session_b)
        } else {
            format!("p2p-bridge-{}-{}", session_b, session_a)
        };

        let (my_session, my_leg, other_session, _other_leg) = if current_session == session_a {
            (
                session_a.clone(),
                leg_a.clone(),
                session_b.clone(),
                leg_b.clone(),
            )
        } else if current_session == session_b {
            (
                session_b.clone(),
                leg_b.clone(),
                session_a.clone(),
                leg_a.clone(),
            )
        } else {
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&session_a) {
                let session_a_clone = session_a.clone();
                handle
                    .send_command(CallCommand::BridgeCrossSession {
                        session_a,
                        leg_a: leg_a.clone(),
                        session_b,
                        leg_b: leg_b.clone(),
                    })
                    .map_err(|e| anyhow!("Failed to forward BridgeCrossSession: {}", e))?;
                info!(session_id = %self.id,
                    "Forwarded BridgeCrossSession to session_a {}",
                    session_a_clone
                );
            }
            return Ok(());
        };

        self.ensure_conference(&conf_id, None).await?;

        let participant_leg = LegId::new(format!("{}-{}", my_session, my_leg));
        self.try_start_and_store_bridge(&conf_id, &participant_leg, "P2P conference media bridge")
            .await;

        if current_session == session_a {
            let registry = &self.server.active_call_registry;
            if let Some(handle) = registry.get_handle(&other_session) {
                let _ = handle.send_command(CallCommand::BridgeCrossSession {
                    session_a: session_a.clone(),
                    leg_a: leg_a.clone(),
                    session_b: session_b.clone(),
                    leg_b: leg_b.clone(),
                });
                info!(session_id = %self.id,
                    session_a = %session_a,
                    session_b = %session_b,
                    "Notified session_b to join P2P conference"
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_disconnect_cancels_blocked_peer_task() {
        let session_cancel = tokio_util::sync::CancellationToken::new();
        let bridge_cancel = session_cancel.child_token();
        let forward = crate::utils::spawn(async {});
        let reverse_cancel = bridge_cancel.child_token();
        let reverse = crate::utils::spawn(async move {
            reverse_cancel.cancelled().await;
        });

        let disconnected = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_bridge_disconnect(session_cancel, bridge_cancel, forward, reverse, None),
        )
        .await
        .expect("bridge monitor must cancel the blocked peer task");

        assert!(disconnected);
    }

    #[test]
    fn await_result_forces_b2bua() {
        assert!(use_b2bua(true, TransferDisposition::AwaitResult));
        assert!(!use_b2bua(true, TransferDisposition::Detach));
    }

    // -------------------------------------------------------------------------
    // parse_transfer_target — pure-function dispatch tests
    //
    // Why these tests didn't exist before:
    //   The target dispatch was inlined inside `handle_blind_transfer` as a
    //   sequence of `starts_with` if-chains.  Without extraction into a
    //   standalone function there was nothing to call in a unit test; the logic
    //   was only reachable through a fully-wired SipSession, so the edge cases
    //   (empty suffix, mixed casing, return_to_ivr param) were never exercised.
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_transfer_target_queue_with_return_to_ivr() {
        let t = parse_transfer_target("queue:support?return_app=ivr&return_target=main");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: Some(ReturnTargetSpec {
                    app_name: "ivr".to_string(),
                    target: Some("main".to_string()),
                    params: HashMap::new(),
                }),
                target_overrides: vec![],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_without_return_to_ivr() {
        let t = parse_transfer_target("queue:support");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: None,
                target_overrides: vec![],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_whitespace_trimmed() {
        let t = parse_transfer_target("queue: sales ");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "sales".to_string(),
                return_app: None,
                target_overrides: vec![],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_target_skillgroup() {
        let t = parse_transfer_target("queue:support?target=skillgroup:sales");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: None,
                target_overrides: vec!["skillgroup:sales".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_target_sip_uri() {
        let t = parse_transfer_target("queue:support?target=sip:agent@pbx.com");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: None,
                target_overrides: vec!["sip:agent@pbx.com".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_multiple_targets() {
        let t = parse_transfer_target(
            "queue:support?target=skillgroup:sales&target=skillgroup:support",
        );
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: None,
                target_overrides: vec![
                    "skillgroup:sales".to_string(),
                    "skillgroup:support".to_string(),
                ],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_target_and_return_to_ivr() {
        let t = parse_transfer_target(
            "queue:support?target=skillgroup:sales&return_app=ivr&return_target=main_menu",
        );
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: Some(ReturnTargetSpec {
                    app_name: "ivr".to_string(),
                    target: Some("main_menu".to_string()),
                    params: HashMap::new(),
                }),
                target_overrides: vec!["skillgroup:sales".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_multiple_targets_and_return_to_ivr() {
        let t = parse_transfer_target(
            "queue:support?target=sip:a@pbx&target=sip:b@pbx&return_app=ivr&return_target=ivr_main",
        );
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_app: Some(ReturnTargetSpec {
                    app_name: "ivr".to_string(),
                    target: Some("ivr_main".to_string()),
                    params: HashMap::new(),
                }),
                target_overrides: vec!["sip:a@pbx".to_string(), "sip:b@pbx".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_ivr() {
        let t = parse_transfer_target("ivr:main");
        assert_eq!(
            t,
            TransferTarget::Ivr {
                name: "main".to_string(),
                params: HashMap::new(),
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_route_point() {
        let t = parse_transfer_target("toivr:39230?order_id=order-001");
        assert_eq!(
            t,
            TransferTarget::RoutePoint {
                name: "39230".to_string(),
                params: HashMap::from([("order_id".to_string(), "order-001".to_string())]),
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_ivr_whitespace_trimmed() {
        let t = parse_transfer_target("ivr: welcome ");
        assert_eq!(
            t,
            TransferTarget::Ivr {
                name: "welcome".to_string(),
                params: HashMap::new(),
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_voicemail() {
        let t = parse_transfer_target("voicemail:1001");
        assert_eq!(
            t,
            TransferTarget::Voicemail {
                extension: "1001".to_string()
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_voicemail_whitespace_trimmed() {
        let t = parse_transfer_target("voicemail: 2001 ");
        assert_eq!(
            t,
            TransferTarget::Voicemail {
                extension: "2001".to_string()
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_empty_voicemail_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("voicemail:");
        assert!(matches!(t, TransferTarget::Sip { .. }));
    }

    #[test]
    fn test_parse_transfer_target_sip_uri_passthrough() {
        let t = parse_transfer_target("sip:1001@pbx.local");
        assert_eq!(
            t,
            TransferTarget::Sip {
                uri: "sip:1001@pbx.local".to_string(),
                return_app: None,
                from_user: None,
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_extracts_from_user() {
        let t = parse_transfer_target("sip:room-123@pbx.example?from_user=relay-caller");
        assert_eq!(
            t,
            TransferTarget::Sip {
                uri: "sip:room-123@pbx.example".to_string(),
                return_app: None,
                from_user: Some("relay-caller".to_string()),
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_tel_uri_passthrough() {
        let t = parse_transfer_target("tel:+15551234567");
        assert_eq!(
            t,
            TransferTarget::Sip {
                uri: "tel:+15551234567".to_string(),
                return_app: None,
                from_user: None,
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_bare_extension_gets_sip_prefix() {
        let t = parse_transfer_target("1001");
        assert_eq!(
            t,
            TransferTarget::Sip {
                uri: "sip:1001".to_string(),
                return_app: None,
                from_user: None,
            }
        );
    }

    /// An empty `queue:` suffix must NOT produce a Queue — it falls through to
    /// Sip so the caller gets a meaningful error from URI parsing rather than a
    /// silent no-op queue lookup.
    #[test]
    fn test_parse_transfer_target_empty_queue_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("queue:");
        // empty name → falls through to Sip
        assert!(matches!(t, TransferTarget::Sip { .. }));
    }

    /// Same guard for `ivr:`.
    #[test]
    fn test_parse_transfer_target_empty_ivr_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("ivr:");
        assert!(matches!(t, TransferTarget::Sip { .. }));
    }

    // -------------------------------------------------------------------------
    // Disposable-channel spin-loop regression check
    //
    // Why the spin loop wasn't caught before:
    //   The pattern `let (_tx, mut rx) = unbounded_channel(); fn(&mut rx)` sends
    //   the sender to `_` (immediately dropped).  Inside `try_single_target` the
    //   tokio::select! polls `rx.recv()` which returns `None` on every tick
    //   because the sender is gone — yet the loop body didn't `break`, so it
    //   spun on the CPU until the parallel `invitation` future completed.
    //   Integration tests exercised the happy-path (call connects quickly)
    //   without measuring early-media forwarding or CPU usage, so the spin was
    //   invisible.  A dropped-sender can be verified as a unit test:
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_dropped_sender_channel_returns_none_immediately() {
        // Demonstrate the old bug: dropped sender → recv() always None.
        let (_tx, mut rx) = mpsc::unbounded_channel::<u32>();
        drop(_tx);
        // recv() on a channel with no senders returns None immediately.
        assert!(
            rx.recv().await.is_none(),
            "dropped sender should yield None"
        );
    }

    #[tokio::test]
    async fn test_live_sender_channel_can_deliver_state() {
        // Demonstrate the fix: a live sender → recv() delivers the message.
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        tx.send(42).unwrap();
        drop(tx);
        assert_eq!(rx.recv().await, Some(42));
    }

    // ── Bridge parsing ─────────────────────────────────────────────────

    #[test]
    fn test_parse_voip_bridge() {
        let target = "voip_bridge:wss://voip.example.com/rooms";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::Bridge {
                endpoint,
                sample_rate,
                codec,
                ..
            } => {
                assert_eq!(endpoint, "wss://voip.example.com/rooms");
                assert_eq!(sample_rate, 8000);
                assert_eq!(codec, "pcm");
            }
            _ => panic!("expected Bridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_voip_bridge_with_query_params() {
        let target = "voip_bridge:wss://room.example.com/ws?token=abc&samplerate=16000&codec=opus&_hdr_Authorization=Bearer+xxx";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::Bridge {
                endpoint,
                headers,
                sample_rate,
                codec,
                ..
            } => {
                assert_eq!(endpoint, "wss://room.example.com/ws?token=abc");
                assert_eq!(sample_rate, 16000);
                assert_eq!(codec, "opus");
                assert_eq!(
                    headers.get("Authorization"),
                    Some(&"Bearer xxx".to_string())
                );
            }
            _ => panic!("expected Bridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_voip_bridge_with_pct_encoded_headers() {
        let target = "voip_bridge:wss://room.example.com/ws?_hdr_X-Custom=hello%20world%26more";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::Bridge {
                headers, endpoint, ..
            } => {
                assert_eq!(endpoint, "wss://room.example.com/ws");
                assert_eq!(
                    headers.get("X-Custom"),
                    Some(&"hello world&more".to_string())
                );
            }
            _ => panic!("expected Bridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_voip_bridge_with_timeout() {
        let target = "voip_bridge:wss://room.example.com/ws?timeout_ms=5000";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::Bridge {
                endpoint,
                timeout_ms,
                ..
            } => {
                assert_eq!(endpoint, "wss://room.example.com/ws");
                assert_eq!(timeout_ms, Some(5000));
            }
            _ => panic!("expected Bridge, got {:?}", parsed),
        }
    }

    /// `_rst_*` params carry the originating IVR node context: they must be
    /// captured into `trace_context` and NEVER leak into the endpoint URL
    /// forwarded to the bridge server.
    #[test]
    fn test_parse_voip_bridge_captures_trace_context() {
        use urlencoding::encode;
        let extra = serde_json::json!({
            "businessnodeid": "1000141102024500020001",
            "nodetype": "menu_tts",
            "nodename": "测试啊，按1转人工，按2挂机",
        });
        let target = format!(
            "bridge:wss://facade.example.com/ivr/tts/bridge/RI_x?samplerate=8000&timeout_ms=30000&return_app=ivr&return_target=lf-step-ivr&_rst_step_id={}&_rst_step_name={}&_rst_extra={}",
            encode("step-1"),
            encode("菜单"),
            encode(&extra.to_string()),
        );
        let parsed = super::parse_transfer_target(&target);
        match parsed {
            TransferTarget::Bridge {
                endpoint,
                trace_context,
                ..
            } => {
                assert_eq!(
                    endpoint, "wss://facade.example.com/ivr/tts/bridge/RI_x",
                    "_rst_* params must not be forwarded to the bridge endpoint"
                );
                let ctx = trace_context.expect("trace context must be captured");
                assert_eq!(ctx.step_id.as_deref(), Some("step-1"));
                assert_eq!(ctx.step_name.as_deref(), Some("菜单"));
                assert_eq!(ctx.extra, Some(extra));
            }
            other => panic!("expected Bridge, got {other:?}"),
        }
    }

    /// A bridge target without `_rst_*` params has no trace context.
    #[test]
    fn test_parse_voip_bridge_without_trace_context() {
        let parsed =
            super::parse_transfer_target("bridge:wss://room.example.com/ws?timeout_ms=1000");
        match parsed {
            TransferTarget::Bridge { trace_context, .. } => {
                assert!(trace_context.is_none());
            }
            other => panic!("expected Bridge, got {other:?}"),
        }
    }

    #[test]
    fn test_voip_bridge_precedence_over_sip() {
        let target = "voip_bridge:wss://room.example.com/ws";
        let parsed = super::parse_transfer_target(target);
        assert!(
            matches!(parsed, TransferTarget::Bridge { .. }),
            "expected Bridge, got {:?}",
            parsed
        );
    }

    #[test]
    fn test_voip_bridge_empty_endpoint_falls_through() {
        let target = "voip_bridge:";
        let parsed = super::parse_transfer_target(target);
        assert!(
            matches!(parsed, TransferTarget::Sip { .. }),
            "empty voip_bridge should fall through to Sip, got {:?}",
            parsed
        );
    }

    // ── E2E integration: WS echo server + raw PCM round-trip ───────────

    /// Spawn a WebSocket echo server on a random local port.
    /// Returns the bound address so the test can connect.
    async fn spawn_ws_echo_server() -> std::net::SocketAddr {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        crate::utils::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let ws_stream = tokio_tungstenite::accept_async(stream).await.unwrap();
                let (ws_write, ws_read) = ws_stream.split();
                // Echo every message back
                crate::utils::spawn(async move {
                    ws_read
                        .map(|msg| {
                            msg.map(|m| {
                                // Echo binary as-is, ignore non-binary
                                if m.is_binary() {
                                    m
                                } else {
                                    tokio_tungstenite::tungstenite::Message::Binary(vec![].into())
                                }
                            })
                        })
                        .forward(ws_write)
                        .await
                        .ok();
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn test_voip_bridge_echo_integration() {
        let addr = spawn_ws_echo_server().await;
        let ws_url = format!("ws://127.0.0.1:{}", addr.port());

        // 1. Verify the target URI parses correctly
        let target = format!("voip_bridge:{ws_url}?_hdr_X-Test=hello&samplerate=8000&codec=pcm");
        let parsed = super::parse_transfer_target(&target);
        let (endpoint, headers, sample_rate, codec) = match parsed {
            TransferTarget::Bridge {
                endpoint,
                headers,
                sample_rate,
                codec,
                ..
            } => (endpoint, headers, sample_rate, codec),
            other => panic!("expected Bridge, got {other:?}"),
        };
        // http::Uri::path() always returns at least "/", so the parsed
        // endpoint will have a trailing slash.
        assert_eq!(endpoint, format!("{ws_url}/"));
        assert_eq!(headers.get("X-Test"), Some(&"hello".to_string()));
        assert_eq!(sample_rate, 8000);
        assert_eq!(codec, "pcm");

        // 2. Connect to the echo server and exchange PCM data
        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect to echo server");
        let (mut ws_write, mut ws_read) = ws_stream.split();

        // Send a PCM16 frame (160 samples at 8kHz = 10ms)
        let tx_samples: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let mut tx_bytes = Vec::with_capacity(tx_samples.len() * 2);
        for s in &tx_samples {
            tx_bytes.extend_from_slice(&s.to_ne_bytes());
        }

        ws_write
            .send(tokio_tungstenite::tungstenite::Message::Binary(
                tx_bytes.clone().into(),
            ))
            .await
            .expect("send PCM data");

        // Receive echoed data
        let echoed = tokio::time::timeout(Duration::from_secs(5), ws_read.next())
            .await
            .expect("timeout waiting for echo")
            .expect("ws stream ended")
            .expect("ws error");

        let rx_bytes = match echoed {
            tokio_tungstenite::tungstenite::Message::Binary(data) => data,
            other => panic!("expected Binary, got {other:?}"),
        };

        // Verify echoed data matches
        assert_eq!(
            rx_bytes.len(),
            tx_bytes.len(),
            "echo should have same byte count"
        );
        let rx_samples: Vec<i16> = rx_bytes
            .chunks_exact(2)
            .map(|c| i16::from_ne_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(rx_samples, tx_samples, "echoed PCM should match original");

        // 3. Close cleanly
        ws_write
            .close()
            .await
            .expect("close WS connection gracefully");
    }

    #[tokio::test]
    async fn test_voip_bridge_resample_linear_8k_to_16k() {
        // Generate 160 samples of 8kHz PCM (= 20ms)
        let input: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let output =
            crate::call::runtime::conference_media_bridge::resample_linear(&input, 8000, 16000);
        // 160 samples at 8kHz → 320 samples at 16kHz (same duration)
        assert_eq!(output.len(), 320, "8k→16k should double sample count");
        // First and last samples should match
        assert_eq!(output[0], input[0]);
        assert_eq!(output[output.len() - 1], input[input.len() - 1]);
    }

    #[tokio::test]
    async fn test_voip_bridge_resample_linear_16k_to_8k() {
        let input: Vec<i16> = (0..320).map(|i| (i * 50) as i16).collect();
        let output =
            crate::call::runtime::conference_media_bridge::resample_linear(&input, 16000, 8000);
        assert_eq!(output.len(), 160, "16k→8k should halve sample count");
    }
}
