use anyhow::{anyhow, bail, Result};
use futures::{SinkExt, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_util::sync::CancellationToken;

use super::SipSession;
use crate::call::quality::{QualitySide, TranscriptSegment};
use crate::media::media_bridge::LegSide;
use crate::rwi::{TranscriptEnded, TranscriptFinal, TranscriptSegmentEvent, TranscriptStarted};

/// Session-held inspection; each selected leg owns one WebSocket task.
pub(crate) struct LiveQuality {
    cancel: CancellationToken,
    connections: Vec<QualityConnection>,
}
impl Drop for LiveQuality {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// One per-side WebSocket task. The child token stops exactly this side on a
/// dynamic re-target; the parent token (dropped/cancelled with the whole
/// inspection) still tears everything down.
pub(crate) struct QualityConnection {
    side: QualitySide,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl LiveQuality {
    fn streaming_sides(&self) -> Vec<QualitySide> {
        self.connections.iter().map(|c| c.side).collect()
    }

    /// Stop the connections whose side is NOT in `keep`. Each cancelled task
    /// is joined under a short deadline — its own close handshake deadline is
    /// independent of this wait, so a slow peer cannot stall re-targeting.
    async fn retain_sides(&mut self, keep: &[LegSide]) {
        let mut retained = Vec::with_capacity(self.connections.len());
        for conn in self.connections.drain(..) {
            let wanted = keep
                .iter()
                .any(|&leg_side| QualitySide::from_leg_side(leg_side) == conn.side);
            if wanted {
                retained.push(conn);
                continue;
            }
            conn.cancel.cancel();
            // Detached completion: the task exits on its own deadline; do not
            // block the session on the peer's close handshake.
        }
        self.connections = retained;
    }

    async fn stop_all(&mut self) {
        for conn in self.connections.drain(..) {
            conn.cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.task).await;
        }
    }
}

/// Owns the decoded allocation for the WebSocket's PCM16LE byte view.
struct PcmBytes(Vec<i16>);

impl From<Vec<i16>> for PcmBytes {
    fn from(mut samples: Vec<i16>) -> Self {
        if cfg!(target_endian = "big") {
            for sample in &mut samples {
                *sample = sample.to_le();
            }
        }
        Self(samples)
    }
}

impl AsRef<[u8]> for PcmBytes {
    fn as_ref(&self) -> &[u8] {
        bytemuck::cast_slice(&self.0)
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename = "start")]
struct QualityStart {
    version: u32,
    sample_rate: u32,
    call_id: String,
    session_id: String,
    side: QualitySide,
    direction: String,
    #[serde(flatten)]
    routing: crate::call::quality::QualityRoutingContext,
    parameters: serde_json::Map<String, Value>,
}

#[derive(Serialize)]
struct ServiceHangupDetail {
    source: &'static str,
    reason: String,
    side: QualitySide,
    detail: Value,
}

/// Wire responses contain no side; the connection carries it alongside the event.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum QualityEvent {
    Text(TranscriptSegment),
    Calltrace(crate::call_errors::TraceEvent),
    Hangup {
        reason: String,
        #[serde(default)]
        detail: Value,
    },
    Error {
        message: String,
    },
    /// Generic session-command passthrough (`mute`/`transfer`/`hold`/`play`/
    /// recording family/...). The inner object is the console wire format
    /// (`{"action": "mute", ...}` — identical to the REST
    /// `POST /calls/active/{id}/commands` body), converted through the shared
    /// console adapter and gated by [`quality_command_allowed`].
    #[cfg(feature = "console")]
    Command {
        command: crate::console::handlers::call_control::CallCommandPayload,
    },
    #[cfg(not(feature = "console"))]
    Command {
        command: Value,
    },
    /// Re-target the audio sides of the running inspection (`"caller"`,
    /// `"callee"`, `"both"`). Any live connection may issue it; it acts on the
    /// session's whole connection set — removed sides stop, missing sides
    /// connect. Missing target legs (callee not answered yet) are skipped, so
    /// the service can retry as the call progresses.
    Switch {
        sides: crate::call::quality::QualitySides,
    },
}

/// Commands a quality service may NOT issue over the inspection channel.
///
/// The call is already established when inspection runs, so the accept phase
/// is meaningless, and the quality lifecycle commands would act on the very
/// connection that issued them (stop would self-terminate mid-dispatch; start
/// would pile a second `LiveQuality` over the running one). Everything else
/// maps through the console adapter exactly as the REST/AMI surfaces do.
#[cfg(feature = "console")]
fn quality_command_allowed(
    command: &crate::console::handlers::call_control::CallCommandPayload,
) -> bool {
    !matches!(
        command,
        crate::console::handlers::call_control::CallCommandPayload::Accept { .. }
            | crate::console::handlers::call_control::CallCommandPayload::StartQualityCheck
            | crate::console::handlers::call_control::CallCommandPayload::StopQualityCheck
    )
}

/// Audit trail for a passthrough quality command: one `TraceKind::Quality`
/// CDR entry recording what the external service asked for and whether the
/// PBX accepted it. Rejections (allowlist) are traced too — they are
/// security-relevant observations.
fn quality_command_trace(
    side: QualitySide,
    action: &str,
    outcome: &str,
) -> crate::call::domain::CallCommand {
    let mut event = crate::call_errors::TraceEvent::new(
        crate::call_errors::TraceKind::Quality,
        format!("quality command {action}: {outcome}"),
    );
    event.side = Some(side.as_str().to_string());
    event.detail = Some(serde_json::json!({
        "source": "realtime_quality",
        "command": action,
        "outcome": outcome,
    }));
    crate::call::domain::CallCommand::Trace { event }
}

/// Convert and dispatch one passthrough command from the quality service.
/// Fire-and-forget, matching the `hangup` precedent: `try_send` success means
/// the session accepted the request, not that the action completed.
#[cfg(feature = "console")]
fn dispatch_quality_command(
    call_id: &str,
    side: QualitySide,
    command: crate::console::handlers::call_control::CallCommandPayload,
    cmd_tx: Option<&tokio::sync::mpsc::Sender<crate::call::domain::CallCommand>>,
) {
    let action = command.action_name();
    if !quality_command_allowed(&command) {
        tracing::warn!(%call_id, ?side, %action, "quality service command rejected by allowlist");
        if let Some(tx) = cmd_tx {
            let _ = tx.try_send(quality_command_trace(side, action, "rejected_by_allowlist"));
        }
        return;
    }
    let converted = match crate::call::adapters::console_to_call_command(command, call_id) {
        Ok(converted) => converted,
        Err(error) => {
            tracing::warn!(%call_id, ?side, %action, %error, "quality service command conversion failed");
            if let Some(tx) = cmd_tx {
                let _ = tx.try_send(quality_command_trace(side, action, "conversion_failed"));
            }
            return;
        }
    };
    match cmd_tx {
        Some(tx) => match tx.try_send(converted) {
            Ok(()) => {
                tracing::info!(
                    audit_event = "quality_command",
                    %call_id,
                    ?side,
                    %action,
                    result = "dispatched",
                    "Quality service command accepted"
                );
                let _ = tx.try_send(quality_command_trace(side, action, "dispatched"));
            }
            Err(error) => {
                tracing::warn!(%call_id, ?side, %action, %error, "quality command dropped");
            }
        },
        None => {
            tracing::warn!(%call_id, ?side, %action, "quality command dropped: no command sender");
        }
    }
}

impl SipSession {
    /// Connect the effective per-call quality endpoint and attach decoded PCM taps.
    pub(super) async fn start_live_quality(&mut self) -> Result<Vec<QualitySide>> {
        // Explicit commands on a cluster child leg fail with a clear reason —
        // the root session at the dispatching node owns the inspection.
        if self
            .context
            .dialplan
            .extensions
            .get::<crate::call::cookie::ClusterChildLeg>()
            .is_some()
        {
            bail!("cluster child leg: quality inspection is owned by the root session");
        }
        if self.live_quality.is_some() {
            bail!("quality inspection already running");
        }
        let (config, routing) = self.effective_quality_config()?;
        let target = quality_target_legs(&config.sides);
        self.live_quality = Some(LiveQuality {
            cancel: self.cancel_token.child_token(),
            connections: Vec::new(),
        });
        let sides = match self.apply_quality_sides(&config, routing, target).await {
            Ok(sides) => sides,
            Err(error) => {
                // Tear the skeleton down so a later start can retry cleanly.
                self.live_quality.take();
                return Err(error);
            }
        };
        if sides.is_empty() {
            // Drop cancels the parent token; no connections were spawned.
            self.live_quality.take();
            return Err(anyhow!(
                "no leg with negotiated media for quality inspection"
            ));
        }
        self.emit_transcript_rwi_event(&TranscriptStarted {
            call_id: self.context.session_id.clone(),
            sides: sides.iter().map(|side| side.as_str().to_string()).collect(),
            provider: None,
        });
        Ok(sides)
    }

    /// Effective per-call quality settings: endpoint config overlaid with the
    /// routing addon's per-call plan, validated.
    fn effective_quality_config(
        &self,
    ) -> Result<(
        crate::call::quality::QualityConfig,
        crate::call::quality::QualityRoutingContext,
    )> {
        let mut config = self
            .server
            .proxy_config
            .load()
            .quality
            .clone()
            .unwrap_or_default();
        let mut routing = crate::call::quality::QualityRoutingContext::default();
        if let Some(plan) = self
            .context
            .dialplan
            .extensions
            .get::<crate::call::quality::QualityPlan>()
        {
            config.apply(plan);
            routing = plan.routing.clone();
        }
        if !config.enabled {
            bail!("quality disabled for this call");
        }
        if config.url.is_empty() {
            bail!("quality service not configured ([proxy.quality])");
        }
        anyhow::ensure!(
            config.receive_timeout_secs > 0,
            "quality receive_timeout_secs must be positive"
        );
        let url = url::Url::parse(&config.url)?;
        if !matches!(url.scheme(), "ws" | "wss") {
            bail!("quality URL must use ws or wss");
        }
        Ok((config, routing))
    }

    /// Re-target the running inspection toward `sides`: stop removed sides,
    /// connect missing ones. No-op for sides already streaming.
    pub(super) async fn switch_quality_sides(
        &mut self,
        sides: crate::call::quality::QualitySides,
    ) -> Result<()> {
        if self.live_quality.is_none() {
            bail!("quality inspection is not running");
        }
        let (config, routing) = self.effective_quality_config()?;
        let target = quality_target_legs(&sides);
        self.apply_quality_sides(&config, routing, target).await?;
        Ok(())
    }

    /// Diff the connection set toward `target` legs. Missing media peers are
    /// tolerated (skipped) so a `switch` issued before the callee leg exists
    /// can be retried as the call progresses.
    async fn apply_quality_sides(
        &mut self,
        config: &crate::call::quality::QualityConfig,
        routing: crate::call::quality::QualityRoutingContext,
        target: &[LegSide],
    ) -> Result<Vec<QualitySide>> {
        let mut request = config.url.as_str().into_client_request()?;
        for (name, value) in &config.headers {
            request
                .headers_mut()
                .insert(name.parse::<http::header::HeaderName>()?, value.parse()?);
        }
        let receive_timeout = Duration::from_secs(config.receive_timeout_secs);
        let call_id = self.context.session_id.clone();
        let session_id = self.id.to_string();
        let direction = self.context.dialplan.direction.to_string();
        let gateway = self.server.rwi_gateway.clone();
        let cmd_tx = self.cmd_tx.clone();

        // Phase 1: stop removed sides, plan the missing ones (parent token
        // children so a whole-teardown still reaches every connection).
        let missing: Vec<(LegSide, CancellationToken)> = {
            let Some(live) = self.live_quality.as_mut() else {
                bail!("quality inspection is not running");
            };
            live.retain_sides(target).await;
            let streaming = live.streaming_sides();
            target
                .iter()
                .filter(|&&leg_side| !streaming.contains(&QualitySide::from_leg_side(leg_side)))
                .map(|&leg_side| (leg_side, live.cancel.child_token()))
                .collect()
        };

        // Phase 2: attach taps and spawn the missing connections (no borrow
        // of `live_quality` held — `media_leg` needs `&self`).
        let mut connected = Vec::with_capacity(missing.len());
        for (leg_side, cancel) in missing {
            let id = match leg_side {
                LegSide::A => "caller",
                LegSide::B => "callee",
            };
            let stream = match self
                .media_leg_for_side(leg_side)
                .ok_or_else(|| anyhow!("No media peer for {}", id))
                .and_then(|peer| peer.pcm_stream(cancel.child_token()))
            {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::debug!(
                        session_id = %session_id,
                        side = ?leg_side,
                        %error,
                        "no PCM tap for leg; skipping side"
                    );
                    continue;
                }
            };
            let spec = ConnectionSpec {
                side: QualitySide::from_leg_side(leg_side),
                call_id: call_id.clone(),
                session_id: session_id.clone(),
                direction: direction.clone(),
                routing: routing.clone(),
                parameters: config.parameters.clone(),
                receive_timeout,
            };
            let task = spawn_quality_connection(
                request.clone(),
                stream,
                spec,
                gateway.clone(),
                cmd_tx.clone(),
                cancel.clone(),
            );
            connected.push(QualityConnection {
                side: QualitySide::from_leg_side(leg_side),
                cancel,
                task,
            });
        }

        // Phase 3: publish the new connections.
        let Some(live) = self.live_quality.as_mut() else {
            bail!("quality inspection is not running");
        };
        live.connections.extend(connected);
        Ok(live.streaming_sides())
    }

    /// Media leg for an inspection side. B ("callee") falls back to the
    /// first non-caller leg: queue dispatch creates the agent leg under a
    /// dynamic id (ring-timeout re-dials get fresh ids), so the fixed
    /// "callee" id does not exist on dispatched calls.
    fn media_leg_for_side(
        &self,
        leg_side: LegSide,
    ) -> Option<crate::media::leg::Leg> {
        let primary = match leg_side {
            LegSide::A => "caller",
            LegSide::B => "callee",
        };
        if let Some(peer) = self.legs.media_leg(&crate::call::domain::LegId::from(primary)) {
            return Some(peer);
        }
        if leg_side == LegSide::B {
            return self
                .legs
                .keys()
                .find(|id| id.as_str() != "caller")
                .and_then(|id| self.legs.media_leg(id));
        }
        None
    }

    /// Stop quality connections and emit
    /// `transcript_ended`. No-op when not running.
    pub(super) async fn stop_live_quality(&mut self, reason: &str) {
        if let Some(mut inspection) = self.live_quality.take() {
            // Connections close concurrently; their handshake deadline is independent
            // of the already-cancelled session token. Join before releasing the owner.
            inspection.stop_all().await;
            if !self.cancel_token.is_cancelled() {
                self.emit_transcript_rwi_event(&TranscriptEnded {
                    call_id: self.context.session_id.clone(),
                    reason: reason.to_string(),
                });
            }
        }
    }
}

/// Per-connection immutable parameters for [`spawn_quality_connection`].
struct ConnectionSpec {
    side: QualitySide,
    call_id: String,
    session_id: String,
    direction: String,
    routing: crate::call::quality::QualityRoutingContext,
    parameters: serde_json::Map<String, Value>,
    receive_timeout: Duration,
}

/// Target legs selected by a sides setting, in stable A-then-B order.
fn quality_target_legs(sides: &crate::call::quality::QualitySides) -> &'static [LegSide] {
    use crate::call::quality::QualitySides;
    match sides {
        QualitySides::Caller => &[LegSide::A],
        QualitySides::Callee => &[LegSide::B],
        QualitySides::Both => &[LegSide::A, LegSide::B],
    }
}

/// Spawn one per-side inspection WebSocket task: connect (3 s budget), upload
/// the start frame + PCM, and translate service events into session
/// commands. The child token stops exactly this connection on a dynamic
/// re-target; the parent token tears the whole inspection down.
fn spawn_quality_connection(
    request: tokio_tungstenite::tungstenite::http::Request<()>,
    stream: crate::media::app_ingress::LegPcmStream,
    spec: ConnectionSpec,
    gateway: Option<crate::rwi::RwiGatewayRef>,
    cmd_tx: Option<tokio::sync::mpsc::Sender<crate::call::domain::CallCommand>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let source_rate = stream.sample_rate();
    let side = spec.side;
    let call_id = spec.call_id;
    let receive_timeout = spec.receive_timeout;
    let start = QualityStart {
        version: 1,
        sample_rate: source_rate,
        call_id: call_id.clone(),
        session_id: spec.session_id,
        side: spec.side,
        direction: spec.direction,
        routing: spec.routing,
        parameters: spec.parameters,
    };
    let message = match serde_json::to_string(&start) {
        Ok(message) => Message::text(message),
        Err(error) => {
            tracing::warn!(%call_id, ?side, %error, "quality start frame serialization failed; side stays dark");
            return tokio::spawn(async {});
        }
    };
    tokio::spawn(async move {
        let connected = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(request),
        )
        .await;
        let ws = match connected {
            Ok(Ok((ws, _))) => ws,
            result => {
                tracing::debug!(?side, ?result, "quality connection ended before upgrade");
                return;
            }
        };
        // Both halves outlive the pipelines so cancellation can still close the socket.
        let (mut writer, mut reader) = ws.split();
        let run = async {
            use crate::call::domain::{CallCommand, HangupCommand};
            let audio = futures::stream::unfold(stream, |mut stream| async move {
                stream.recv().await.map(|pcm| (pcm, stream))
            })
            .map(move |pcm| -> Result<Message> {
                if pcm.frame.sample_rate != source_rate {
                    bail!("quality PCM sample rate changed within connection");
                }
                let bytes = bytes::Bytes::from_owner(PcmBytes::from(pcm.frame.samples));
                Ok(Message::Binary(bytes))
            })
            .boxed();
            let messages = futures::stream::once(async { Ok(message) }).chain(audio);
            tokio::pin!(messages);
            let mut writer = (&mut writer).sink_map_err(anyhow::Error::from);
            let upload = writer.send_all(&mut messages);
            tokio::pin!(upload);
            let mut receive_deadline = tokio::time::Instant::now() + receive_timeout;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep_until(receive_deadline) => bail!("quality service receive timeout after {} seconds", receive_timeout.as_secs()),
                    result = &mut upload => { result?; break; },
                    message = reader.next() => {
                        if matches!(&message, Some(Ok(_))) {
                            receive_deadline = tokio::time::Instant::now() + receive_timeout;
                        }
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let event: QualityEvent = match serde_json::from_str(&text) {
                                    Ok(event) => event,
                                    Err(error) => {
                                        tracing::warn!(%call_id, ?side, %error, "invalid quality event; ignoring message");
                                        continue;
                                    }
                                };
                                match event {
                                    QualityEvent::Text(seg) => {
                                        if seg.end_ms < seg.start_ms { bail!("invalid quality text timestamps"); }
                                        let Some(gateway) = gateway.as_ref() else { continue; };
                                        if seg.partial {
                                            gateway.read().send_to_owner_and_fan_out(&TranscriptSegmentEvent {
                                                call_id: call_id.clone(),
                                                side: side.as_str().to_string(),
                                                text: seg.text,
                                                partial: seg.partial,
                                                start_ms: seg.start_ms,
                                                end_ms: seg.end_ms,
                                                lang: seg.lang,
                                            });
                                        } else {
                                            gateway.read().send_to_owner_and_fan_out(&TranscriptFinal {
                                                call_id: call_id.clone(),
                                                side: side.as_str().to_string(),
                                                text: seg.text,
                                                start_ms: seg.start_ms,
                                                end_ms: seg.end_ms,
                                                lang: seg.lang,
                                                provider: None,
                                            });
                                        }
                                    }
                                    QualityEvent::Error { message } => bail!("quality service: {message}"),
                                    QualityEvent::Calltrace(mut event) => {
                                        // The owning session stamps ts; the connection owns side.
                                        event.side = Some(side.as_str().to_string());
                                        if let Some(tx) = &cmd_tx {
                                            if let Err(error) = tx.try_send(CallCommand::Trace { event }) {
                                                tracing::warn!(%call_id, ?side, %error, "quality trace command dropped");
                                            }
                                        }
                                    }
                                    QualityEvent::Hangup { reason, detail } => {
                                        tracing::info!(%call_id, ?side, %reason, "quality service requested hangup");
                                        if let Some(tx) = &cmd_tx {
                                            let mut command = HangupCommand::local(
                                                "realtime_quality",
                                                Some(crate::callrecord::CallRecordHangupReason::Autohangup),
                                                None,
                                            );
                                            command.trace_detail = Some(serde_json::to_value(ServiceHangupDetail {
                                                source: "realtime_quality", reason, side, detail,
                                            })?);
                                            if let Err(error) = tx.try_send(CallCommand::Hangup(command)) {
                                                tracing::warn!(%call_id, ?side, %error, "quality hangup command dropped");
                                            }
                                        } else {
                                            tracing::warn!(%call_id, ?side, "quality hangup command dropped: no command sender");
                                        }
                                    }
                                    #[cfg(feature = "console")]
                                    QualityEvent::Command { command } => {
                                        dispatch_quality_command(&call_id, side, command, cmd_tx.as_ref());
                                    }
                                    #[cfg(not(feature = "console"))]
                                    QualityEvent::Command { command } => {
                                        tracing::warn!(%call_id, ?side, ?command, "quality service command ignored: console feature disabled");
                                    }
                                    QualityEvent::Switch { sides } => {
                                        tracing::info!(%call_id, ?side, ?sides, "quality service switched inspection sides");
                                        if let Some(tx) = &cmd_tx {
                                            let _ = tx.try_send(quality_command_trace(
                                                side,
                                                "switch_sides",
                                                &format!("dispatched:{sides:?}"),
                                            ));
                                            if let Err(error) = tx.try_send(CallCommand::QualitySwitchSides { sides }) {
                                                tracing::warn!(%call_id, ?side, %error, "quality switch command dropped");
                                            }
                                        } else {
                                            tracing::warn!(%call_id, ?side, "quality switch command dropped: no command sender");
                                        }
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(_))) => {
                                // The next read poll drives Tungstenite's automatic Pong reply.
                            },
                            Some(Ok(Message::Close(_))) => break,
                            None => break,
                            Some(Err(e)) => return Err(e.into()),
                            _ => {},
                        }
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        let result = run.await;
        // The completed pipeline has dropped its audio stream. No business messages or
        // dialog lookups are needed from here, even after session teardown.
        if let Err(error) = result {
            tracing::debug!(?side, %error, "quality connection ended");
        }
        let mut ws = writer
            .reunite(reader)
            .expect("quality WebSocket halves belong together");
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            // Sink close also flushes a queued peer Close reply; keep reading
            // to finish the handshake, ignoring any late business messages.
            SinkExt::close(&mut ws).await?;
            ws.try_for_each(|_| futures::future::ready(Ok(()))).await
        })
        .await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::{tungstenite::protocol::Role, WebSocketStream};

    #[test]
    fn pcm_bytes_preserve_allocation_and_little_endian_samples() {
        let samples = vec![i16::MIN, -1, 0, 1, 0x1234, i16::MAX];
        let pointer = samples.as_ptr().cast::<u8>();
        let expected: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let bytes = bytes::Bytes::from_owner(PcmBytes::from(samples));
        assert_eq!(bytes.as_ptr(), pointer);
        assert_eq!(bytes.as_ref(), expected.as_slice());
        let shared = bytes.clone();
        drop(bytes);
        assert_eq!(shared.as_ptr(), pointer);
        assert_eq!(shared.as_ref(), expected.as_slice());
    }

    #[tokio::test(start_paused = true)]
    async fn quality_blocked_upload_remains_cancellable_without_write_deadline() {
        let (client, _peer) = tokio::io::duplex(64);
        let ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut writer = ws.sink_map_err(anyhow::Error::from);
        let mut messages = futures::stream::iter([Ok(Message::Binary(vec![0; 4096].into()))]);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            trigger.cancel();
        });
        let started = tokio::time::Instant::now();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {},
            result = writer.send_all(&mut messages) => panic!("blocked upload must remain pending: {result:?}"),
        }
        assert_eq!(started.elapsed(), Duration::from_secs(6));
    }

    #[tokio::test(start_paused = true)]
    async fn quality_send_all_allows_idle_audio() {
        let (client, _peer) = tokio::io::duplex(64);
        let ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut writer = ws.sink_map_err(anyhow::Error::from);
        let messages = futures::stream::once(async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            Ok(Message::Binary(vec![0].into()))
        });
        tokio::pin!(messages);
        writer
            .send_all(&mut messages)
            .await
            .expect("idle audio is not a blocked write");
    }

    #[cfg(feature = "console")]
    #[test]
    fn quality_command_wire_format_parses() {
        let event: QualityEvent = serde_json::from_str(
            r#"{"type":"command","command":{"action":"mute","track_id":"caller"}}"#,
        )
        .expect("command event parses");
        let QualityEvent::Command { command } = event else {
            panic!("expected Command variant");
        };
        assert!(matches!(
            command,
            crate::console::handlers::call_control::CallCommandPayload::Mute { .. }
        ));
    }

    #[test]
    fn quality_switch_wire_format_parses() {
        let event: QualityEvent =
            serde_json::from_str(r#"{"type":"switch","sides":"both"}"#).expect("switch parses");
        let QualityEvent::Switch { sides } = event else {
            panic!("expected Switch variant");
        };
        assert_eq!(sides, crate::call::quality::QualitySides::Both);
        // Unknown side names keep failing (closed enum).
        assert!(
            serde_json::from_str::<QualityEvent>(r#"{"type":"switch","sides":"ringing"}"#).is_err()
        );
    }

    #[test]
    fn quality_switch_command_wire_roundtrip() {
        use crate::call::quality::QualitySides;
        let cmd = crate::call::domain::CallCommand::QualitySwitchSides {
            sides: QualitySides::Callee,
        };
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["type"], "quality_switch_sides");
        assert_eq!(json["sides"], "callee");
        let back: crate::call::domain::CallCommand = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            crate::call::domain::CallCommand::QualitySwitchSides {
                sides: QualitySides::Callee
            }
        ));
        assert!(cmd.requires_media());
    }

    #[tokio::test]
    async fn live_quality_retain_sides_stops_removed_and_keeps_running() {
        use crate::media::media_bridge::LegSide;
        let parent = CancellationToken::new();
        let mut live = LiveQuality {
            cancel: parent.clone(),
            connections: Vec::new(),
        };
        let mut spawn_conn = |side, parent: &CancellationToken| {
            let cancel = parent.child_token();
            let task_cancel = cancel.clone();
            let task = tokio::spawn(async move {
                tokio::select! {
                    _ = task_cancel.cancelled() => {},
                    _ = futures::future::pending::<()>() => {},
                }
            });
            QualityConnection { side, cancel, task }
        };
        live.connections
            .push(spawn_conn(QualitySide::Caller, &parent));
        live.connections
            .push(spawn_conn(QualitySide::Callee, &parent));

        // Keep only the caller side → the callee connection is stopped.
        live.retain_sides(&[LegSide::A]).await;
        assert_eq!(live.streaming_sides(), vec![QualitySide::Caller]);

        // A no-op retain (already streaming) keeps the connection.
        live.retain_sides(&[LegSide::A]).await;
        assert_eq!(live.streaming_sides(), vec![QualitySide::Caller]);

        // Full teardown via the parent token: Drop cancels every child.
        drop(live);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    #[cfg(feature = "console")]
    #[test]
    fn quality_command_allowlist_blocks_accept_and_lifecycle() {
        use crate::console::handlers::call_control::CallCommandPayload;
        for rejected in [
            serde_json::json!({"action":"accept"}),
            serde_json::json!({"action":"start_quality_check"}),
            serde_json::json!({"action":"stop_quality_check"}),
        ] {
            let command: CallCommandPayload = serde_json::from_value(rejected).unwrap();
            assert!(
                !quality_command_allowed(&command),
                "{command:?} must be rejected by the allowlist"
            );
        }
        for allowed in [
            serde_json::json!({"action":"mute","track_id":"caller"}),
            serde_json::json!({"action":"unmute","track_id":"callee"}),
            serde_json::json!({"action":"transfer","target":"sip:1002@example.com"}),
            serde_json::json!({"action":"hold","leg_id":"caller"}),
            serde_json::json!({"action":"play","source":{"type":"tone","frequency":440,"duration_ms":1000}}),
            serde_json::json!({"action":"hangup","reason":null,"code":null,"initiator":null}),
        ] {
            let command: CallCommandPayload = serde_json::from_value(allowed).unwrap();
            assert!(
                quality_command_allowed(&command),
                "{command:?} must be allowed by the allowlist"
            );
        }
    }

    #[cfg(feature = "console")]
    #[tokio::test]
    async fn quality_command_dispatch_converts_and_audits() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let command: crate::console::handlers::call_control::CallCommandPayload =
            serde_json::from_str(r#"{"action":"mute","track_id":"caller"}"#).unwrap();
        dispatch_quality_command("call-1", QualitySide::Caller, command, Some(&tx));
        // First message: the converted console command.
        match rx.recv().await.expect("converted command") {
            crate::call::domain::CallCommand::MuteTrack { track_id } => {
                assert_eq!(track_id, "caller");
            }
            other => panic!("expected MuteTrack, got {other:?}"),
        }
        // Second message: the audit trace entry.
        match rx.recv().await.expect("audit trace") {
            crate::call::domain::CallCommand::Trace { event } => {
                assert_eq!(event.side.as_deref(), Some("caller"));
                let detail = event.detail.expect("trace detail");
                assert_eq!(detail["source"], "realtime_quality");
                assert_eq!(detail["command"], "mute");
                assert_eq!(detail["outcome"], "dispatched");
            }
            other => panic!("expected Trace, got {other:?}"),
        }
    }

    #[cfg(feature = "console")]
    #[tokio::test]
    async fn quality_command_dispatch_rejects_lifecycle_commands() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let command: crate::console::handlers::call_control::CallCommandPayload =
            serde_json::from_str(r#"{"action":"start_quality_check"}"#).unwrap();
        dispatch_quality_command("call-1", QualitySide::Callee, command, Some(&tx));
        // Only the rejection audit trace arrives; the command itself is dropped.
        match rx.recv().await.expect("rejection trace") {
            crate::call::domain::CallCommand::Trace { event } => {
                assert_eq!(event.side.as_deref(), Some("callee"));
                assert_eq!(
                    event.detail.expect("trace detail")["outcome"],
                    "rejected_by_allowlist"
                );
            }
            other => panic!("expected Trace, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no command may be dispatched");
    }
}
