//! Realtime bridge core loop — transport between a realtime WebSocket and
//! the call leg's audio, free of `SipSession` types so it is directly
//! testable against an in-process mock server.
//!
//! The session (see `proxy/proxy_call/sip_session/realtime_bridge.rs`) wires
//! this up: MediaBridge leg tap → uplink pump, [`Playout`] impl → leg
//! egress, [`BridgeOutput`]s → RWI gateway + command channel.

use super::{DownlinkEvent, RealtimeParams, RealtimeProtocol, UplinkMessage};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// One uplink PCM frame (mono, at `sample_rate`) ready for the protocol.
#[derive(Debug, Clone)]
pub struct UplinkPcm {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

/// Why the realtime bridge ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeEndReason {
    /// Cancelled locally (app exit, stop command, call teardown).
    Cancelled,
    /// The endpoint closed the WebSocket.
    WsClosed,
    /// The endpoint errored (protocol/transport).
    WsError(String),
}

/// Output of the bridge loop, consumed by the session-side task.
#[derive(Debug)]
pub enum BridgeOutput {
    /// RWI `realtime_event` to broadcast.
    Event {
        kind: String,
        data: serde_json::Value,
    },
    /// Caller-side DTMF digit reported by the endpoint.
    Dtmf { digit: char },
}

/// Playout sink abstraction so the loop is testable without a MediaBridge.
#[async_trait::async_trait]
pub trait Playout: Send {
    /// Write one PCM frame (already at the WS sample rate) to the caller.
    async fn write(&mut self, pcm: &[i16]);
    /// Mute immediately (barge-in): drop any buffered audio.
    async fn mute(&mut self);
}

/// Everything the bridge loop needs.
pub struct BridgeIo {
    pub ws_write: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    pub ws_read: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    pub uplink_rx: mpsc::Receiver<UplinkPcm>,
    pub playout: Box<dyn Playout>,
    pub cancel: CancellationToken,
}

/// Outcome of the bridge loop handed back to the session.
pub struct BridgeRun {
    pub reason: BridgeEndReason,
}

/// Core realtime bridge loop.
///
/// Sends the protocol handshake, pumps uplink PCM → WS, decodes WS → playout
/// + outputs, and honors barge-in. Returns when cancelled or the socket
/// closes/errors.
pub async fn run_realtime_bridge(
    protocol: Box<dyn RealtimeProtocol>,
    params: &RealtimeParams,
    mut io: BridgeIo,
    output_tx: mpsc::Sender<BridgeOutput>,
) -> BridgeRun {
    let (mut ws_write, mut ws_read) = (io.ws_write, io.ws_read);
    let ws_rate = protocol.sample_rate(params);

    // ── Handshake (session.update for OpenAI; nothing for raw PCM) ──────
    for msg in protocol.session_open_messages(params) {
        if send_uplink(&mut ws_write, &msg).await.is_err() {
            return BridgeRun {
                reason: BridgeEndReason::WsError("handshake send failed".into()),
            };
        }
    }
    let _ = output_tx
        .send(BridgeOutput::Event {
            kind: "connected".into(),
            data: serde_json::json!({
                "protocol": protocol.kind().as_str(),
                "sample_rate": ws_rate,
            }),
        })
        .await;

    let mut muted = false;
    let reason = 'bridge: loop {
        tokio::select! {
            _ = io.cancel.cancelled() => break 'bridge BridgeEndReason::Cancelled,

            frame = io.uplink_rx.recv() => {
                let Some(frame) = frame else {
                    // Leg tap detached (call ended) — stop quietly.
                    break 'bridge BridgeEndReason::Cancelled;
                };
                let pcm = if frame.sample_rate == ws_rate {
                    frame.samples
                } else {
                    crate::call::runtime::conference_media_bridge::resample_linear(
                        &frame.samples,
                        frame.sample_rate,
                        ws_rate,
                    )
                };
                let mut send_failed = false;
                for msg in protocol.encode_uplink(&pcm) {
                    if send_uplink(&mut ws_write, &msg).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
                if send_failed {
                    break 'bridge BridgeEndReason::WsError("uplink send failed".into());
                }
            }

            msg = ws_read.next() => {
                let Some(msg) = msg else {
                    break 'bridge BridgeEndReason::WsClosed;
                };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => break 'bridge BridgeEndReason::WsError(e.to_string()),
                };
                let events = match &msg {
                    Message::Text(text) => protocol.decode_text(text),
                    Message::Binary(bytes) => protocol.decode_binary(bytes),
                    Message::Ping(_) | Message::Pong(_) => Vec::new(),
                    Message::Close(_) => break 'bridge BridgeEndReason::WsClosed,
                    _ => Vec::new(),
                };
                for event in events {
                    match event {
                        DownlinkEvent::AudioDelta {
                            pcm,
                            sample_rate: rate,
                        } => {
                            if muted {
                                continue; // barge-in: playout stays muted until next utterance cycle
                            }
                            let rate = if rate == 0 { ws_rate } else { rate };
                            let pcm = if rate == ws_rate {
                                pcm
                            } else {
                                crate::call::runtime::conference_media_bridge::resample_linear(
                                    &pcm, rate, ws_rate,
                                )
                            };
                            io.playout.write(&pcm).await;
                        }
                        DownlinkEvent::SpeechStarted => {
                            muted = true;
                            io.playout.mute().await;
                            let mut send_failed = false;
                            if let Some(cancel) = protocol.cancel_response()
                                && send_uplink(&mut ws_write, &cancel).await.is_err()
                            {
                                send_failed = true;
                            }
                            if send_failed {
                                break 'bridge BridgeEndReason::WsError("cancel send failed".into());
                            }
                            let _ = output_tx
                                .send(BridgeOutput::Event {
                                    kind: "barge_in".into(),
                                    data: serde_json::json!({ "reason": "speech_started" }),
                                })
                                .await;
                        }
                        DownlinkEvent::SpeechStopped => {
                            muted = false;
                            let _ = output_tx
                                .send(BridgeOutput::Event {
                                    kind: "speech_stopped".into(),
                                    data: serde_json::json!({}),
                                })
                                .await;
                        }
                        DownlinkEvent::Dtmf { digit } => {
                            let _ = output_tx.send(BridgeOutput::Dtmf { digit }).await;
                        }
                        other => {
                            let (kind, data) = match other {
                                DownlinkEvent::SessionReady => ("session_ready", serde_json::json!({})),
                                DownlinkEvent::TranscriptDelta { text } => {
                                    ("transcript_delta", serde_json::json!({ "text": text }))
                                }
                                DownlinkEvent::TranscriptFinal { text } => {
                                    ("transcript_final", serde_json::json!({ "text": text }))
                                }
                                DownlinkEvent::FunctionCall { name, arguments, call_id } => (
                                    "function_call",
                                    serde_json::json!({
                                        "name": name,
                                        "arguments": arguments,
                                        "call_id": call_id,
                                    }),
                                ),
                                DownlinkEvent::Error { message } => {
                                    ("error", serde_json::json!({ "message": message }))
                                }
                                _ => unreachable!("audio/vad/dtmf handled above"),
                            };
                            let _ = output_tx.send(BridgeOutput::Event { kind: kind.to_string(), data }).await;
                        }
                    }
                }
            }
        }
    };

    info!(reason = ?reason, "realtime bridge loop ended");
    let _ = output_tx
        .send(BridgeOutput::Event {
            kind: "disconnected".into(),
            data: serde_json::json!({ "reason": format!("{reason:?}") }),
        })
        .await;
    BridgeRun { reason }
}

async fn send_uplink(
    ws: &mut futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    msg: &UplinkMessage,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let wire = match msg {
        UplinkMessage::Text(text) => Message::Text(text.clone().into()),
        UplinkMessage::Binary(bytes) => Message::Binary(bytes.clone().into()),
    };
    ws.send(wire).await
}

/// Connect to the realtime endpoint. Credentials travel in the upgrade
/// request headers built by the protocol adapter — never as URL query.
pub async fn connect_realtime_ws(
    protocol: &dyn RealtimeProtocol,
    params: &RealtimeParams,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    anyhow::Error,
> {
    let url = protocol.connect_url(params);
    let mut request = IntoClientRequest::into_client_request(&url)
        .map_err(|e| anyhow::anyhow!("invalid realtime url '{url}': {e}"))?;
    for (name, value) in protocol.upgrade_headers(params) {
        let parsed_name = name
            .parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
            .map_err(|e| anyhow::anyhow!("invalid header name '{name}': {e}"))?;
        let parsed_value = value
            .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
            .map_err(|e| anyhow::anyhow!("invalid header value for '{name}': {e}"))?;
        request.headers_mut().insert(parsed_name, parsed_value);
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (ws, _resp) = if let Some(ms) = params.timeout_ms {
        tokio::time::timeout(std::time::Duration::from_millis(ms), connect)
            .await
            .map_err(|_| anyhow::anyhow!("realtime connect timed out after {}ms", ms))??
    } else {
        connect
            .await
            .map_err(|e| anyhow::anyhow!("realtime connect failed: {e}"))?
    };
    Ok(ws)
}
