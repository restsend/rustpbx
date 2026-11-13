use anyhow::{Result, anyhow, bail};
use std::time::Duration;
use futures::{SinkExt, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;

use super::SipSession;
use crate::call::quality::{
    QualitySide, TranscriptSegment,
};
use crate::media::media_bridge::LegSide;
use crate::rwi::{
    TranscriptEnded, TranscriptFinal, TranscriptSegmentEvent, TranscriptStarted,
};

/// Session-held inspection; each selected leg owns one WebSocket task.
pub(crate) struct LiveQuality {
    cancel: CancellationToken,
    connections: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for LiveQuality {
    fn drop(&mut self) { self.cancel.cancel(); }
}

/// Owns the decoded allocation for the WebSocket's PCM16LE byte view.
struct PcmBytes(Vec<i16>);

impl From<Vec<i16>> for PcmBytes {
    fn from(mut samples: Vec<i16>) -> Self {
        if cfg!(target_endian = "big") {
            for sample in &mut samples { *sample = sample.to_le(); }
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
    Hangup { reason: String, #[serde(default)] detail: Value },
    Error { message: String },
}

impl SipSession {
    /// Connect the effective per-call quality endpoint and attach decoded PCM taps.
    pub(super) async fn start_live_quality(&mut self) -> Result<Vec<QualitySide>> {
        let mut config = self.server.proxy_config.load().quality.clone().unwrap_or_default();
        let mut routing = crate::call::quality::QualityRoutingContext::default();
        if let Some(plan) = self.context.dialplan.extensions.get::<crate::call::quality::QualityPlan>() {
            config.apply(plan);
            routing = plan.routing.clone();
        }
        if !config.enabled { return Err(anyhow!("quality disabled for this call")); }
        if config.url.is_empty() { return Err(anyhow!("quality service not configured ([proxy.quality])")); }
        anyhow::ensure!(config.receive_timeout_secs > 0, "quality receive_timeout_secs must be positive");
        let receive_timeout = Duration::from_secs(config.receive_timeout_secs);
        // Attach only selected legs; tolerate a missing side on single-leg calls.
        let mut sides = Vec::new();
        let mut streams = Vec::new();
        use crate::call::quality::QualitySides;
        let selected: &[LegSide] = match config.sides {
            QualitySides::Caller => &[LegSide::A],
            QualitySides::Callee => &[LegSide::B],
            QualitySides::Both => &[LegSide::A, LegSide::B],
        };
        for &leg_side in selected {
            let id = match leg_side { LegSide::A => "caller", LegSide::B => "callee" };
            let stream = self.media_leg(&crate::call::domain::LegId::from(id))
                .ok_or_else(|| anyhow!("No media peer for {}", id))
                .and_then(|peer| peer.pcm_stream(self.cancel_token.child_token()));
            match stream {
                Ok(stream) => {
                    sides.push(QualitySide::from_leg_side(leg_side));
                    streams.push((QualitySide::from_leg_side(leg_side), stream));
                }
                Err(e) => {
                    tracing::debug!(
                        session_id = %self.id,
                        side = ?leg_side,
                        error = %e,
                        "no PCM tap for leg; skipping side"
                    );
                }
            }
        }
        if sides.is_empty() {
            return Err(anyhow!("no leg with negotiated media for quality inspection"));
        }

        let call_id = self.context.session_id.clone();
        let direction = self.context.dialplan.direction.to_string();
        let url = url::Url::parse(&config.url)?;
        if !matches!(url.scheme(), "ws" | "wss") { bail!("quality URL must use ws or wss"); }
        let mut request = config.url.as_str().into_client_request()?;
        for (name, value) in &config.headers {
            request.headers_mut().insert(
                name.parse::<http::header::HeaderName>()?, value.parse()?,
            );
        }
        let cancel = self.cancel_token.child_token();
        let mut connections = Vec::new();
        self.emit_typed_rwi_event(&TranscriptStarted {
            call_id: call_id.clone(),
            sides: sides.iter().map(|side| side.as_str().to_string()).collect(),
            provider: None,
        });
        for (side, stream) in streams {
            let request = request.clone();
            let cancel = cancel.clone();
            let gateway = self.server.rwi_gateway.clone();
            let cmd_tx = self.cmd_tx.clone();
            let call_id = call_id.clone();
            let direction = direction.clone();
            let source_rate = stream.sample_rate();
            let start = QualityStart {
                version: 1, sample_rate: source_rate, call_id: call_id.clone(), session_id: self.id.to_string(), side,
                direction: direction.clone(), routing: routing.clone(), parameters: config.parameters.clone(),
            };
            let message = Message::text(serde_json::to_string(&start)?);
            connections.push(tokio::spawn(async move {
                let connected = tokio::time::timeout(
                    Duration::from_secs(3), tokio_tungstenite::connect_async(request),
                ).await;
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
                    }).map(move |pcm| -> Result<Message> {
                        if pcm.frame.sample_rate != source_rate {
                            bail!("quality PCM sample rate changed within connection");
                        }
                        let bytes = bytes::Bytes::from_owner(PcmBytes::from(pcm.frame.samples));
                        Ok(Message::Binary(bytes))
                    }).boxed();
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
                                                    gateway.read().send_to_owner(&TranscriptSegmentEvent {
                                                        call_id: call_id.clone(),
                                                        side: side.as_str().to_string(),
                                                        text: seg.text,
                                                        partial: seg.partial,
                                                        start_ms: seg.start_ms,
                                                        end_ms: seg.end_ms,
                                                        lang: seg.lang,
                                                    });
                                                } else {
                                                    gateway.read().send_to_owner(&TranscriptFinal {
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
                let mut ws = writer.reunite(reader).expect("quality WebSocket halves belong together");
                let _ = tokio::time::timeout(Duration::from_secs(1), async {
                    // Sink close also flushes a queued peer Close reply; keep reading
                    // to finish the handshake, ignoring any late business messages.
                    SinkExt::close(&mut ws).await?;
                    ws.try_for_each(|_| futures::future::ready(Ok(()))).await
                }).await;
            }));
        }
        self.live_quality = Some(LiveQuality { cancel, connections });

        Ok(sides)
    }

    /// Stop quality connections and emit
    /// `transcript_ended`. No-op when not running.
    pub(super) async fn stop_live_quality(&mut self, reason: &str) {
        if let Some(mut inspection) = self.live_quality.take() {
            inspection.cancel.cancel();
            // Connections close concurrently; their handshake deadline is independent
            // of the already-cancelled session token. Join before releasing the owner.
            for task in inspection.connections.drain(..) {
                let _ = task.await;
            }
            if !self.cancel_token.is_cancelled() {
                self.emit_typed_rwi_event(&TranscriptEnded {
                    call_id: self.context.session_id.clone(),
                    reason: reason.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};

    #[test]
    fn pcm_bytes_preserve_allocation_and_little_endian_samples() {
        let samples = vec![i16::MIN, -1, 0, 1, 0x1234, i16::MAX];
        let pointer = samples.as_ptr().cast::<u8>();
        let expected: Vec<u8> = samples.iter().flat_map(|sample| sample.to_le_bytes()).collect();
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
        writer.send_all(&mut messages).await.expect("idle audio is not a blocked write");
    }
}
