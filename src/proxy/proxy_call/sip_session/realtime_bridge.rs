//! Session-side wiring for the realtime (AI voice) bridge.
//!
//! The transport core lives in [`crate::call::realtime::bridge`] (testable
//! without a session); this module provides the MediaBridge-backed
//! [`Playout`] and the `SipSession` command handlers: MediaBridge leg tap →
//! uplink pump, leg egress → playout, bridge outputs → RWI gateway +
//! command channel, lifecycle (hangup on disconnect) and cleanup on
//! hangup / app replacement.

use crate::call::realtime::bridge::{
    BridgeEndReason, BridgeIo, BridgeOutput, Playout, UplinkPcm, connect_realtime_ws,
    run_realtime_bridge,
};
use crate::call::realtime::{RealtimeParams, RealtimeProtocol};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// MediaBridge-backed playout. Holds a cloned [`Leg`](rustpbx_media::leg::Leg)
/// (interior `Arc`s — cheap, `&self` APIs only) so the bridge task never
/// needs the owning `MediaBridge`:
///
/// - write: arm a `ChannelAudioSource` sink via `Leg::play`, re-armed on demand
/// - mute (barge-in): switch the leg egress to `EgressSource::Silence` —
///   the old sink (and its up-to-256-frame buffer) is abandoned outright,
///   so no stale AI audio can leak into the call; the next `write` after
///   `SpeechStopped` re-arms a fresh sink.
pub struct MediaBridgePlayout {
    pub leg: crate::media::leg::Leg,
    pub sample_rate: u32,
    tx: Option<mpsc::Sender<Vec<i16>>>,
}

impl MediaBridgePlayout {
    pub fn new(leg: crate::media::leg::Leg, sample_rate: u32) -> Self {
        Self {
            leg,
            sample_rate,
            tx: None,
        }
    }

    async fn ensure_sink(&mut self) {
        if self.tx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel(256);
        let source = Box::new(crate::media::audio_source::ChannelAudioSource::new(
            rx,
            self.sample_rate,
        ));
        match self.leg.play(source, false, None).await {
            Ok(()) => self.tx = Some(tx),
            Err(e) => warn!(error = %e, "realtime: failed to arm playout sink"),
        }
    }
}

#[async_trait::async_trait]
impl Playout for MediaBridgePlayout {
    async fn write(&mut self, pcm: &[i16]) {
        if self.tx.is_none() {
            self.ensure_sink().await;
        }
        if let Some(tx) = self.tx.as_mut()
            && tx.send(pcm.to_vec()).await.is_err()
        {
            // Sink ended (egress switched elsewhere) — re-arm next frame.
            self.tx = None;
        }
    }

    async fn mute(&mut self) {
        // Abandon the writer, then force the egress to digital silence: the
        // stale ChannelAudioSource buffer is dropped with the egress switch.
        self.tx = None;
        if let Err(e) = self
            .leg
            .set_egress_source(crate::media::egress::EgressSource::Silence)
            .await
        {
            warn!(error = %e, "realtime: playout mute failed");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Session wiring — `impl SipSession` handlers for the realtime commands.
// ═══════════════════════════════════════════════════════════════════════

/// Cancellable handle for the active realtime bridge, owned by the session.
pub(crate) struct RealtimeBridgeHandle {
    pub cancel: CancellationToken,
    /// `true` when an unexpected endpoint close should hang the call up
    /// (from `RealtimeParams::hangup_on_disconnect`).
    pub hangup_on_disconnect: bool,
}

use super::session::SipSession;
use crate::call::domain::{CallCommand, HangupCascade, HangupCommand, HangupInitiator};
use crate::call_errors::{ErrSeverity, TraceEvent, TraceKind};
use crate::rwi::RealtimeEvent;

impl SipSession {
    /// Cancel the active realtime bridge without any hangup side effects —
    /// used on call teardown, `StopApp` and app replacement. Idempotent.
    pub(crate) fn teardown_realtime_bridge(&mut self, why: &str) {
        if let Some(handle) = self.realtime_bridge.take() {
            handle.cancel.cancel();
            info!(session_id = %self.id, why, "realtime: bridge torn down");
        }
    }

    /// `CallCommand::RealtimeStart` — connect to the endpoint, wire the
    /// MediaBridge tap (uplink) and playout (downlink), and run the bridge
    /// loop in the background. One bridge per session: a second start
    /// replaces the first.
    pub(crate) async fn handle_realtime_start(
        &mut self,
        mut params: crate::call::realtime::RealtimeParams,
    ) -> anyhow::Result<()> {
        // Replace any active bridge first (idempotent teardown).
        self.handle_realtime_stop(Some("replaced".to_string()))
            .await
            .ok();

        let protocol = crate::call::realtime::protocol_for(&params);
        // Env fallback (OPENAI_API_KEY / REALTIME_API_KEY) — config preset
        // key, when present, already lives in `params.api_key`.
        if params.api_key.is_none() {
            params.api_key = params.effective_api_key();
        }

        // MediaBridge is mandatory: the realtime app is app-anchored, so the
        // caller leg's decoded PCM and egress live there.
        let bridge = self
            .bridge()
            .ok_or_else(|| anyhow::anyhow!("realtime bridge requires MediaBridge"))?
            .leg(crate::media::media_bridge::LegSide::A)
            .ok_or_else(|| anyhow::anyhow!("realtime bridge: no caller leg"))?;
        let uplink_stream = self
            .bridge()
            .ok_or_else(|| anyhow::anyhow!("realtime bridge requires MediaBridge"))?
            .leg_pcm_stream(crate::media::media_bridge::LegSide::A)?;

        let ws = connect_realtime_ws(protocol.as_ref(), &params).await?;
        let (ws_write, ws_read) = ws.split();
        info!(
            session_id = %self.id,
            endpoint = %params.url,
            protocol = params.protocol.as_str(),
            "realtime: WebSocket connected"
        );

        let cancel = CancellationToken::new();
        let ws_rate = protocol.sample_rate(&params);
        let hangup_on_disconnect = params.hangup_on_disconnect;
        let protocol_kind = params.protocol.as_str();
        let url_host = params
            .url
            .split("//")
            .nth(1)
            .and_then(|h| h.split('/').next())
            .unwrap_or("")
            .to_string();

        // ── Uplink pump: MediaBridge tap → protocol (silence skipped —
        //    server VAD providers don't need comfort noise) ────────────────
        let (uplink_tx, uplink_rx) = mpsc::channel::<UplinkPcm>(64);
        {
            let pump_cancel = cancel.clone();
            crate::utils::spawn(async move {
                let mut stream = uplink_stream;
                loop {
                    tokio::select! {
                        _ = pump_cancel.cancelled() => break,
                        frame = stream.recv() => match frame {
                            Some(f) => {
                                if f.silence {
                                    continue;
                                }
                                if uplink_tx
                                    .send(UplinkPcm {
                                        samples: f.frame.samples,
                                        sample_rate: f.frame.sample_rate,
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            None => break,
                        },
                    }
                }
            });
        }

        // ── Output consumer: RWI broadcast + DTMF relay ────────────────────
        let (output_tx, mut output_rx) = mpsc::channel::<BridgeOutput>(64);
        {
            let gateway = self.server.rwi_gateway.clone();
            let call_id = self.context.session_id.clone();
            let cmd_tx = self.cmd_tx.clone();
            let consumer_cancel = cancel.clone();
            crate::utils::spawn(async move {
                loop {
                    let output = tokio::select! {
                        _ = consumer_cancel.cancelled() => break,
                        output = output_rx.recv() => match output {
                            Some(o) => o,
                            None => break,
                        },
                    };
                    match output {
                        BridgeOutput::Event { kind, data } => {
                            if let Some(gw) = &gateway {
                                gw.read().broadcast(&RealtimeEvent {
                                    call_id: call_id.clone(),
                                    kind,
                                    data,
                                });
                            }
                        }
                        BridgeOutput::Dtmf { digit } => {
                            if let Some(tx) = &cmd_tx {
                                let _ = tx
                                    .send(CallCommand::SendDtmf {
                                        leg_id: crate::call::domain::LegId::from("caller"),
                                        digits: digit.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
            });
        }

        // ── Bridge loop task — on end, notify the session ──────────────────
        {
            let loop_cancel = cancel.clone();
            let cmd_tx = self.cmd_tx.clone();
            let protocol = protocol;
            crate::utils::spawn(async move {
                let io = BridgeIo {
                    ws_write,
                    ws_read,
                    uplink_rx,
                    playout: Box::new(MediaBridgePlayout::new(bridge, ws_rate)),
                    cancel: loop_cancel.clone(),
                };
                let run = run_realtime_bridge(protocol, &params, io, output_tx).await;
                // Tear down pump + consumer, then notify the session. The
                // session decides whether an endpoint close hangs the call up.
                loop_cancel.cancel();
                let unexpected = run.reason != BridgeEndReason::Cancelled;
                if let Some(tx) = &cmd_tx {
                    let _ = tx
                        .send(CallCommand::RealtimeStop {
                            reason: if unexpected {
                                Some(format!("{:?}", run.reason))
                            } else {
                                None
                            },
                        })
                        .await;
                    if unexpected && hangup_on_disconnect {
                        let _ = tx
                            .send(CallCommand::Hangup(HangupCommand {
                                leg_id: None,
                                cascade: HangupCascade::All,
                                initiator: HangupInitiator::System {
                                    reason: crate::call::domain::SystemHangupReason::InternalError,
                                    details: Some("realtime endpoint disconnected".into()),
                                },
                                reason: Some(crate::callrecord::CallRecordHangupReason::BySystem),
                                code: None,
                                rtp_timeout_side: None,
                            }))
                            .await;
                    }
                }
            });
        }

        self.realtime_bridge = Some(RealtimeBridgeHandle {
            cancel,
            hangup_on_disconnect,
        });

        self.record_trace(
            TraceEvent::new(
                TraceKind::Ivr,
                format!("Realtime bridge started ({protocol_kind})"),
            )
            .severity(ErrSeverity::Info)
            .detail(serde_json::json!({
                "url_host": url_host,
                "protocol": protocol_kind,
            })),
        );
        Ok(())
    }

    /// `CallCommand::RealtimeStop` — tear down the active bridge. When
    /// `reason` is `Some`, the stop came from the bridge task itself
    /// (endpoint close/error): with `hangup_on_disconnect` the call hangs up.
    /// `None` is the app's clean exit — no hangup.
    pub(crate) async fn handle_realtime_stop(
        &mut self,
        reason: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(handle) = self.realtime_bridge.take() else {
            return Ok(());
        };
        handle.cancel.cancel();
        info!(session_id = %self.id, reason = ?reason, "realtime: bridge stopped");
        self.record_trace(
            TraceEvent::new(
                TraceKind::Ivr,
                format!(
                    "Realtime bridge stopped{}",
                    reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ),
            )
            .severity(ErrSeverity::Info),
        );
        if reason.is_some() && handle.hangup_on_disconnect {
            self.handle_hangup(&HangupCommand {
                leg_id: None,
                cascade: HangupCascade::All,
                initiator: HangupInitiator::System {
                    reason: crate::call::domain::SystemHangupReason::InternalError,
                    details: Some("realtime endpoint disconnected".into()),
                },
                reason: Some(crate::callrecord::CallRecordHangupReason::BySystem),
                code: None,
                rtp_timeout_side: None,
            })
            .await;
        }
        Ok(())
    }
}
