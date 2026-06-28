use super::SipSession;
use crate::call::domain::{CallCommand, LegId, LegState};
use crate::callrecord::CallRecordHangupReason;
use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use rsipstack::dialog::dialog::DialogState;
use rsipstack::sip::StatusCode;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

// Re-export for peer access
use crate::call::runtime::conference_media_bridge::AudioReceiver;
use crate::proxy::proxy_call::sip_session::PeerConnectionAudioReceiver;
use std::collections::HashMap;
use std::time::Duration;

/// Parsed representation of a transfer target URI.
///
/// Extracted so that the string-prefix dispatch in `handle_blind_transfer` is
/// type-safe and unit-testable independently of `SipSession`.
#[derive(Debug, PartialEq)]
pub(crate) enum TransferTarget {
    Queue {
        name: String,
        return_ivr: Option<String>,
        target_overrides: Vec<String>,
    },
    Ivr {
        name: String,
    },
    Voicemail {
        extension: String,
    },
    Conference {
        id: String,
    },
    /// WebSocket + PCM real-time VoIP bridge.
    VoipBridge {
        endpoint: String,
        headers: HashMap<String, String>,
        sample_rate: u32,
        codec: String,
        timeout_ms: Option<u64>,
    },
    Sip(String),
}

/// Parse a raw transfer target string into a typed `TransferTarget`.
///
/// Delegates prefix dispatch to [`TransferEndpoint::parse`] and enriches the
/// result with transfer‑specific data (queue query params, voip_bridge options).
/// Bare strings without a recognised prefix get `sip:` prepended.
pub(crate) fn parse_transfer_target(target: &str) -> TransferTarget {
    // 1. `voip_bridge:` is too complex for TransferEndpoint – parse inline.
    if let Some(rest) = target.strip_prefix("voip_bridge:") {
        let raw = rest.trim();
        if !raw.is_empty() {
            let mut sample_rate = 8000u32;
            let mut codec = "pcm".to_string();
            let mut timeout_ms = None;
            let mut headers = HashMap::new();
            let mut passthrough_params = Vec::new();

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
                return TransferTarget::VoipBridge {
                    endpoint: ep,
                    headers,
                    sample_rate,
                    codec,
                    timeout_ms,
                };
            }
        }
    }

    // 2. Delegate to the canonical prefix parser.
    if let Some(ep) = crate::call::TransferEndpoint::parse(target) {
        return match ep {
            // Queue: also extract query params (return_ivr, target overrides).
            crate::call::TransferEndpoint::Queue(mut raw_name) => {
                let query_str = raw_name.find('?').map(|pos| {
                    let qs = raw_name[pos + 1..].to_string();
                    raw_name.truncate(pos);
                    qs
                });
                let queue_name = raw_name.trim().to_string();
                if queue_name.is_empty() {
                    TransferTarget::Sip(format!("sip:{}", target))
                } else {
                    let mut return_ivr = None;
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
                                "return_ivr" => return_ivr = Some(decoded),
                                "target" => target_overrides.push(decoded),
                                _ => {}
                            }
                        }
                    }
                    TransferTarget::Queue {
                        name: queue_name,
                        return_ivr,
                        target_overrides,
                    }
                }
            }
            crate::call::TransferEndpoint::Ivr(name) => TransferTarget::Ivr { name },
            crate::call::TransferEndpoint::Voicemail(extension) => {
                TransferTarget::Voicemail { extension }
            }
            crate::call::TransferEndpoint::Conference(id) => TransferTarget::Conference { id },
            // Plain SIP/TEL URI – ensure at least the `sip:` scheme.
            crate::call::TransferEndpoint::Uri(uri) => {
                let sip = if uri.starts_with("sip:") || uri.starts_with("tel:") {
                    uri
                } else {
                    format!("sip:{}", uri)
                };
                TransferTarget::Sip(sip)
            }
        };
    }

    // 3. Fallback (should not normally happen).
    TransferTarget::Sip(format!("sip:{}", target))
}

impl SipSession {
    pub(super) async fn handle_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        attended: bool,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        info!(%leg_id, %target, %attended, "Handling transfer");

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
                info!(
                    "Attended transfer initiated - consultation call should be created externally"
                );
            }
        } else {
            self.handle_blind_transfer(leg_id, target, callee_state_rx)
                .await?;
        }

        Ok(())
    }

    pub(super) async fn handle_blind_transfer(
        &mut self,
        leg_id: LegId,
        target: String,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        match parse_transfer_target(&target) {
            TransferTarget::Queue {
                name,
                return_ivr,
                target_overrides,
            } => {
                info!(%leg_id, queue = %name, ?return_ivr, overrides = %target_overrides.len(), "Handling queue transfer");
                self.handle_queue_transfer(
                    leg_id,
                    &name,
                    return_ivr,
                    target_overrides,
                    callee_state_rx,
                )
                .await
            }
            TransferTarget::Ivr { name } => {
                info!(%leg_id, ivr = %name, "Handling IVR transfer by starting IvrApp");
                self.start_ivr_app(&name).await
            }
            TransferTarget::Voicemail { extension } => {
                info!(%leg_id, %extension, "Handling voicemail transfer by starting VoicemailApp");
                self.start_voicemail_app(&extension).await
            }
            TransferTarget::Conference { id } => {
                info!(%leg_id, conf_id = %id, "Handling conference transfer by starting ConferenceApp");
                self.start_conference_app(&id).await
            }
            TransferTarget::VoipBridge {
                endpoint,
                headers,
                sample_rate,
                codec,
                timeout_ms,
            } => {
                info!(%leg_id, endpoint = %endpoint, sample_rate, codec = %codec, "Handling VoipBridge transfer");
                self.connect_voip_bridge(
                    leg_id,
                    endpoint.clone(),
                    headers.clone(),
                    sample_rate,
                    codec.clone(),
                    timeout_ms,
                )
                .await
            }
            TransferTarget::Sip(refer_to_str) => {
                let realm = self.server.proxy_config.select_realm("");
                let normalized = crate::call::build_sip_uri(&refer_to_str, &realm);
                let refer_to_uri = rsipstack::sip::Uri::try_from(normalized.as_str())
                    .map_err(|e| anyhow!("Invalid transfer target URI: {}", e))?;

                if !self.server.proxy_config.blind_transfer_use_refer {
                    info!(%leg_id, target = %refer_to_str, "Blind transfer via B-leg INVITE (B2BUA)");
                    let location = crate::call::Location {
                        aor: refer_to_uri,
                        ..Default::default()
                    };
                    let result = self
                        .try_single_target(&location, callee_state_rx, None)
                        .await;
                    return result.map_err(|(code, text, reason)| {
                        anyhow!(
                            "B-leg transfer failed: {} {} - {}",
                            code,
                            text,
                            reason.unwrap_or_default()
                        )
                    });
                }

                // SIP REFER path
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

                info!(%leg_id, target = %refer_to_str, "Sending REFER for blind transfer");

                match self
                    .server_dialog
                    .refer(refer_to_uri, Some(headers), None)
                    .await
                {
                    Ok(Some(response)) => {
                        let status = response.status_code.code();
                        info!(status = %status, "REFER response received");

                        let reason = Self::refer_reason_for_status(status).map(String::from);
                        self.emit_refer_event(
                            status,
                            reason,
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;

                        match status {
                            202 => {
                                info!("REFER accepted (202), transfer in progress");
                                self.update_leg_state(&leg_id, LegState::Ending);
                            }
                            100..=199 => {
                                info!("REFER received provisional response {}", status);
                            }
                            405 | 420 | 501 => {
                                warn!(status = %status, "REFER not supported by peer, needs 3PCC fallback");
                                return Err(anyhow!(
                                    "REFER not supported by peer ({}), needs 3PCC fallback",
                                    status
                                ));
                            }
                            _ if status >= 400 => {
                                warn!(status = %status, "REFER rejected");
                                return Err(anyhow!("REFER rejected with status {}", status));
                            }
                            _ => {
                                warn!(status = %status, "Unexpected REFER response");
                                return Err(anyhow!("Unexpected REFER response: {}", status));
                            }
                        }
                    }
                    Ok(None) => {
                        warn!("REFER timed out, no response received");
                        self.emit_refer_event(
                            408,
                            Some("timeout".to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("REFER timed out"));
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to send REFER");
                        self.emit_refer_event(
                            500,
                            Some(e.to_string()),
                            crate::call::domain::ReferNotifyEventType::ReferResponse,
                        )
                        .await;
                        return Err(anyhow!("Failed to send REFER: {}", e));
                    }
                }

                info!(
                    "Blind transfer initiated — call will be transferred to {}",
                    refer_to_str
                );
                Ok(())
            }
        }
    }

    pub(crate) async fn handle_queue_transfer(
        &mut self,
        leg_id: LegId,
        queue_name: &str,
        return_ivr: Option<String>,
        target_overrides: Vec<String>,
        callee_state_rx: &mut mpsc::UnboundedReceiver<DialogState>,
    ) -> Result<()> {
        info!(%leg_id, queue = %queue_name, ?return_ivr, overrides = %target_overrides.len(), "Starting queue transfer");

        let queue_config = self
            .server
            .data_context
            .resolve_queue_config(queue_name)
            .map_err(|e| anyhow!("Failed to resolve queue config: {}", e))?;

        let queue_config = match queue_config {
            Some(config) => config,
            None => {
                return Err(anyhow!("Queue '{}' not found", queue_name));
            }
        };

        let mut queue_plan = queue_config
            .to_queue_plan()
            .map_err(|e| anyhow!("Invalid queue config: {}", e))?;

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
                info!(
                    overrides = %locations.len(),
                    "Queue transfer: overriding targets from query params"
                );
                queue_plan.dial_strategy = Some(DialStrategy::Sequential(locations));
            }
        }

        let return_ivr_fallback_audio: Option<String> = if return_ivr.is_some() {
            queue_plan.failure_audio.clone().or_else(|| {
                if let Some(crate::call::QueueFallbackAction::Failure(
                    crate::call::FailureAction::PlayThenHangup { ref audio_file, .. },
                )) = queue_plan.fallback
                {
                    Some(audio_file.clone())
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(ref ivr_name) = return_ivr {
            info!(
                queue = %queue_name,
                ivr = %ivr_name,
                "Queue transfer: will return to IVR on fallback"
            );
            let name = ivr_name.clone();
            queue_plan.fallback = Some(crate::call::QueueFallbackAction::Failure(
                crate::call::FailureAction::Transfer(crate::call::TransferEndpoint::Ivr(name)),
            ));
            queue_plan.failure_audio = return_ivr_fallback_audio;
        }

        let queue_result = self.execute_queue(&queue_plan, callee_state_rx).await;

        match queue_result {
            Ok(()) => {
                info!(queue = %queue_name, "Queue transfer completed successfully");
                Ok(())
            }
            Err((code, text, reason)) => {
                warn!(
                    queue = %queue_name,
                    code = %code,
                    text = %text,
                    ?reason,
                    "Queue transfer failed"
                );
                if self.server_dialog.state().is_confirmed() {
                    self.meta.last_error =
                        Some((StatusCode::Other(code, text.clone()), reason.clone()));
                    self.meta
                        .hangup_reason
                        .get_or_insert(CallRecordHangupReason::Failed);
                    self.pending_hangup.insert(self.server_dialog.id());
                    self.cancel_token.cancel();
                    info!(
                        queue = %queue_name,
                        code = %code,
                        text = %text,
                        ?reason,
                        "Queue transfer failed after caller was answered; hanging up caller dialog"
                    );
                    return Ok(());
                }
                Err(anyhow!(
                    "Queue transfer failed: {} {} {:?}",
                    code,
                    text,
                    reason
                ))
            }
        }
    }

    pub(crate) async fn start_ivr_app(&self, ivr_name: &str) -> Result<()> {
        let ivr_file = self.server.data_context.resolve_ivr_file(ivr_name);
        info!(ivr = %ivr_name, file = %ivr_file, "Starting IVR application");
        let params = Some(serde_json::json!({"file": ivr_file}));
        self.ensure_app_running("ivr", params, &format!("IVR '{}'", ivr_name))
            .await
    }

    pub(crate) async fn start_voicemail_app(&self, extension: &str) -> Result<()> {
        info!(extension = %extension, "Starting voicemail application");
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
        info!(conf_id = %conf_id, "Starting conference application");
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
    pub(crate) async fn connect_voip_bridge(
        &mut self,
        leg_id: LegId,
        endpoint: String,
        _headers: HashMap<String, String>,
        sample_rate: u32,
        codec: String,
        timeout_ms: Option<u64>,
    ) -> Result<()> {
        info!(%leg_id, endpoint = %endpoint, sample_rate, codec = %codec, "Connecting VoipBridge");

        // ── 1. Establish WebSocket connection ──────────────────────────
        let ws_connect = tokio_tungstenite::connect_async(&endpoint);
        let (ws_stream, _) = if let Some(ms) = timeout_ms {
            tokio::time::timeout(Duration::from_millis(ms), ws_connect)
                .await
                .map_err(|_| anyhow!("VoipBridge connection timed out after {}ms", ms))?
                .map_err(|e| anyhow!("Failed to connect VoipBridge WebSocket: {}", e))?
        } else {
            ws_connect
                .await
                .map_err(|e| anyhow!("Failed to connect VoipBridge WebSocket: {}", e))?
        };
        info!("VoipBridge WebSocket connected to {}", endpoint);
        let (mut ws_write, mut ws_read) = ws_stream.split();

        // ── 2. Get the leg's media peer, track sender & PC ───────────
        let peer = self
            .legs
            .get_peer(&leg_id)
            .cloned()
            .or_else(|| self.caller_peer().cloned())
            .ok_or_else(|| anyhow!("No media peer available"))?;

        let tracks = peer.get_tracks().await;
        let mut audio_sender = None;
        let mut pc = None;
        for t in &tracks {
            let guard = t.lock().await;
            if audio_sender.is_none() {
                audio_sender = guard.get_sender();
            }
            if pc.is_none() {
                pc = guard.get_peer_connection().await;
            }
        }
        let audio_sender = audio_sender.ok_or_else(|| anyhow!("No track sender for VoipBridge"))?;
        let pc = pc.ok_or_else(|| anyhow!("No PeerConnection for VoipBridge"))?;

        // ── 3. Create audio receiver (call → raw PCM) ────────────────
        let decoder = self
            .create_audio_decoder()
            .ok_or_else(|| anyhow!("Failed to create audio decoder"))?;
        let mut audio_receiver = PeerConnectionAudioReceiver::new(pc, decoder);

        // ── 4. Determine codec type for the forward encoder ───────────
        let codec_type = match codec.as_str() {
            "pcm" | "pcmu" => audio_codec::CodecType::PCMU,
            "pcma" | "g711" => audio_codec::CodecType::PCMA,
            "opus" => audio_codec::CodecType::Opus,
            "g722" => audio_codec::CodecType::G722,
            _ => {
                let negotiated = self.leg_negotiated_codec(&leg_id);
                info!(?negotiated, "Using negotiated codec");
                negotiated
            }
        };
        let ws_sample_rate = if sample_rate == 0 { 8000 } else { sample_rate };

        // ── 5. Cancellation token (parent = session cancel) ──────────
        let cancel_token = self.cancel_token.child_token();

        // ── 6. Forward loop: WS raw PCM16 → inject into call ─────────
        let forward_cancel = cancel_token.child_token();
        let forward_handle = {
            let leg_id = leg_id.clone();
            crate::utils::spawn(async move {
                use audio_codec::create_encoder;
                use rustrtc::media::{AudioFrame as RtcAudioFrame, MediaSample};

                let mut encoder = create_encoder(codec_type);
                let enc_sample_rate = encoder.sample_rate();
                let clock_rate = codec_type.clock_rate() as u32;
                let payload_type = codec_type.payload_type();
                let samples_per_frame = (enc_sample_rate * 20 / 1000) as usize;
                let rtp_ticks_per_frame = clock_rate * 20 / 1000;

                let mut rtp_ts: u32 = rand::random();
                let mut seq: u16 = rand::random();
                let mut buf: Vec<i16> = Vec::new();

                loop {
                    tokio::select! {
                        biased;
                        _ = forward_cancel.cancelled() => {
                            info!(%leg_id, "VoipBridge forward loop cancelled");
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
                                        let chunk = if ws_sample_rate != enc_sample_rate {
                                            crate::call::runtime::conference_media_bridge::resample_linear(
                                                &chunk, ws_sample_rate, enc_sample_rate,
                                            )
                                        } else {
                                            chunk
                                        };
                                        let encoded = encoder.encode(&chunk);
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
                                        if audio_sender.send(MediaSample::Audio(frame)).await.is_err() {
                                            warn!(%leg_id, "VoipBridge forward: audio sender closed");
                                            return;
                                        }
                                        rtp_ts = rtp_ts.wrapping_add(rtp_ticks_per_frame);
                                        seq = seq.wrapping_add(1);
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    info!(%leg_id, "VoipBridge WS closed remotely");
                                    break;
                                }
                                Some(Err(e)) => {
                                    warn!(%leg_id, "VoipBridge WS read error: {}", e);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            })
        };

        // ── 7. Reverse loop: call audio → raw PCM16 → WS ─────────────
        let reverse_cancel = cancel_token.child_token();
        let reverse_handle = {
            let leg_id = leg_id.clone();
            crate::utils::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = reverse_cancel.cancelled() => {
                            info!(%leg_id, "VoipBridge reverse loop cancelled");
                            break;
                        }
                        pcm = audio_receiver.recv() => {
                            match pcm {
                                Some(frame) => {
                                    let samples = if frame.sample_rate != ws_sample_rate {
                                        crate::call::runtime::conference_media_bridge::resample_linear(
                                            &frame.samples, frame.sample_rate, ws_sample_rate,
                                        )
                                    } else {
                                        frame.samples
                                    };
                                    let mut bytes = Vec::with_capacity(samples.len() * 2);
                                    for s in &samples {
                                        bytes.extend_from_slice(&s.to_ne_bytes());
                                    }
                                    if ws_write.send(Message::Binary(bytes.into())).await.is_err() {
                                        warn!(%leg_id, "VoipBridge WS write failed");
                                        break;
                                    }
                                }
                                None => {
                                    info!(%leg_id, "VoipBridge audio receiver closed");
                                    break;
                                }
                            }
                        }
                    }
                }
            })
        };

        // ── 8. Register tasks for session cleanup ────────────────────
        self.legs
            .tasks
            .entry(leg_id.clone())
            .or_default()
            .push(forward_handle);
        self.legs
            .tasks
            .entry(leg_id.clone())
            .or_default()
            .push(reverse_handle);

        // ── 9. Store bridge reference on session ─────────────────────
        self.conference_bridge = crate::call::runtime::SessionConferenceBridge {
            bridge_handle: Some(crate::call::runtime::ConferenceBridgeHandle {
                _tasks: vec![],
                cancel_token,
            }),
            conf_id: Some(format!("voip-bridge-{}", self.id.0)),
        };

        info!(%leg_id, endpoint = %endpoint, "VoipBridge established");
        Ok(())
    }

    pub(super) fn build_replaces_header(&self) -> Option<String> {
        let dialog_id = self.server_dialog.id();

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

        self.handle_blind_transfer(leg_id, refer_target, callee_state_rx)
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
        info!(%consult_leg, "Completing attended transfer");

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
        info!(%consult_leg, "Canceling attended transfer");

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
            info!("Attended transfer canceled, original call resumed");
        }

        Ok(())
    }

    pub(super) async fn handle_transfer_complete_cross_session(
        &mut self,
        from_session: String,
        leg_id: LegId,
        into_conference: String,
    ) -> Result<()> {
        info!(
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

        info!(
            session_id = %self.id,
            leg_id = %leg_id,
            leg_state = ?leg.state,
            "Found leg for cross-session migration"
        );

        let conference_manager = &self.server.conference_manager;
        let conf_id = crate::call::runtime::ConferenceId::from(into_conference.as_str());

        // Use a consistent composite leg_id for both conference registration and
        // media bridge start — previously the two used different IDs causing a mismatch.
        let participant_leg = LegId::new(format!("{}-{}", from_session, leg_id));
        conference_manager
            .add_participant(&conf_id, participant_leg.clone())
            .await
            .map_err(|e| anyhow!("Failed to add leg to conference: {}", e))?;

        info!(
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

        info!(
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
                info!(
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
                info!(
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

    // -------------------------------------------------------------------------
    // parse_transfer_target — pure-function dispatch tests
    //
    // Why these tests didn't exist before:
    //   The target dispatch was inlined inside `handle_blind_transfer` as a
    //   sequence of `starts_with` if-chains.  Without extraction into a
    //   standalone function there was nothing to call in a unit test; the logic
    //   was only reachable through a fully-wired SipSession, so the edge cases
    //   (empty suffix, mixed casing, return_ivr param) were never exercised.
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_transfer_target_queue_with_return_ivr() {
        let t = parse_transfer_target("queue:support?return_ivr=main");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_ivr: Some("main".to_string()),
                target_overrides: vec![],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_without_return_ivr() {
        let t = parse_transfer_target("queue:support");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_ivr: None,
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
                return_ivr: None,
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
                return_ivr: None,
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
                return_ivr: None,
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
                return_ivr: None,
                target_overrides: vec![
                    "skillgroup:sales".to_string(),
                    "skillgroup:support".to_string(),
                ],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_target_and_return_ivr() {
        let t = parse_transfer_target("queue:support?target=skillgroup:sales&return_ivr=main_menu");
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_ivr: Some("main_menu".to_string()),
                target_overrides: vec!["skillgroup:sales".to_string()],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_queue_with_multiple_targets_and_return_ivr() {
        let t = parse_transfer_target(
            "queue:support?target=sip:a@pbx&target=sip:b@pbx&return_ivr=ivr_main",
        );
        assert_eq!(
            t,
            TransferTarget::Queue {
                name: "support".to_string(),
                return_ivr: Some("ivr_main".to_string()),
                target_overrides: vec!["sip:a@pbx".to_string(), "sip:b@pbx".to_string(),],
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_ivr() {
        let t = parse_transfer_target("ivr:main");
        assert_eq!(
            t,
            TransferTarget::Ivr {
                name: "main".to_string()
            }
        );
    }

    #[test]
    fn test_parse_transfer_target_ivr_whitespace_trimmed() {
        let t = parse_transfer_target("ivr: welcome ");
        assert_eq!(
            t,
            TransferTarget::Ivr {
                name: "welcome".to_string()
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
        assert!(matches!(t, TransferTarget::Sip(_)));
    }

    #[test]
    fn test_parse_transfer_target_sip_uri_passthrough() {
        let t = parse_transfer_target("sip:1001@pbx.local");
        assert_eq!(t, TransferTarget::Sip("sip:1001@pbx.local".to_string()));
    }

    #[test]
    fn test_parse_transfer_target_tel_uri_passthrough() {
        let t = parse_transfer_target("tel:+15551234567");
        assert_eq!(t, TransferTarget::Sip("tel:+15551234567".to_string()));
    }

    #[test]
    fn test_parse_transfer_target_bare_extension_gets_sip_prefix() {
        let t = parse_transfer_target("1001");
        assert_eq!(t, TransferTarget::Sip("sip:1001".to_string()));
    }

    /// An empty `queue:` suffix must NOT produce a Queue — it falls through to
    /// Sip so the caller gets a meaningful error from URI parsing rather than a
    /// silent no-op queue lookup.
    #[test]
    fn test_parse_transfer_target_empty_queue_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("queue:");
        // empty name → falls through to Sip
        assert!(matches!(t, TransferTarget::Sip(_)));
    }

    /// Same guard for `ivr:`.
    #[test]
    fn test_parse_transfer_target_empty_ivr_suffix_falls_through_to_sip() {
        let t = parse_transfer_target("ivr:");
        assert!(matches!(t, TransferTarget::Sip(_)));
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

    // ── VoipBridge parsing ─────────────────────────────────────────────────

    #[test]
    fn test_parse_voip_bridge() {
        let target = "voip_bridge:wss://voip.example.com/rooms";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::VoipBridge {
                endpoint,
                sample_rate,
                codec,
                ..
            } => {
                assert_eq!(endpoint, "wss://voip.example.com/rooms");
                assert_eq!(sample_rate, 8000);
                assert_eq!(codec, "pcm");
            }
            _ => panic!("expected VoipBridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_voip_bridge_with_query_params() {
        let target = "voip_bridge:wss://room.example.com/ws?token=abc&samplerate=16000&codec=opus&_hdr_Authorization=Bearer+xxx";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::VoipBridge {
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
            _ => panic!("expected VoipBridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_voip_bridge_with_pct_encoded_headers() {
        let target = "voip_bridge:wss://room.example.com/ws?_hdr_X-Custom=hello%20world%26more";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::VoipBridge {
                headers, endpoint, ..
            } => {
                assert_eq!(endpoint, "wss://room.example.com/ws");
                assert_eq!(
                    headers.get("X-Custom"),
                    Some(&"hello world&more".to_string())
                );
            }
            _ => panic!("expected VoipBridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_voip_bridge_with_timeout() {
        let target = "voip_bridge:wss://room.example.com/ws?timeout_ms=5000";
        let parsed = super::parse_transfer_target(target);
        match parsed {
            TransferTarget::VoipBridge {
                endpoint,
                timeout_ms,
                ..
            } => {
                assert_eq!(endpoint, "wss://room.example.com/ws");
                assert_eq!(timeout_ms, Some(5000));
            }
            _ => panic!("expected VoipBridge, got {:?}", parsed),
        }
    }

    #[test]
    fn test_voip_bridge_precedence_over_sip() {
        let target = "voip_bridge:wss://room.example.com/ws";
        let parsed = super::parse_transfer_target(target);
        assert!(
            matches!(parsed, TransferTarget::VoipBridge { .. }),
            "expected VoipBridge, got {:?}",
            parsed
        );
    }

    #[test]
    fn test_voip_bridge_empty_endpoint_falls_through() {
        let target = "voip_bridge:";
        let parsed = super::parse_transfer_target(target);
        assert!(
            matches!(parsed, TransferTarget::Sip(_)),
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
            TransferTarget::VoipBridge {
                endpoint,
                headers,
                sample_rate,
                codec,
                ..
            } => (endpoint, headers, sample_rate, codec),
            other => panic!("expected VoipBridge, got {other:?}"),
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
