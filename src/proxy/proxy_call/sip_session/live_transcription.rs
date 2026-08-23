//! Live transcription orchestration for a call session.
//!
//! `StartTranscription` attaches decoded-PCM taps to both MediaBridge legs
//! (the same `leg_pcm_stream` primitive the conference / supervisor mixers
//! use), forwards frames to a [`TranscriptionProvider`], and publishes every
//! recognized segment as a `transcript_segment` RWI event. Subscribers (the
//! CC addon's SSE endpoint, webhooks, RWI sessions) receive them through the
//! gateway's normal dispatch paths.
//!
//! Start/stop are reference-counted: the pump starts on the first
//! `StartTranscription` and stops when the matching number of
//! `StopTranscription` commands arrive (or the call ends).

use anyhow::{Result, anyhow};
use tokio_util::sync::CancellationToken;

use super::SipSession;
use crate::call::transcription::remote::RemoteStreamingProvider;
use crate::call::transcription::{
    SidePcmFrame, TranscriptSide, TranscriptionEvent, TranscriptionProvider,
};
use crate::media::media_bridge::LegSide;
use crate::rwi::{TranscriptEnded, TranscriptError, TranscriptSegmentEvent, TranscriptStarted};

/// Session-held live-transcription state.
pub(crate) struct LiveTranscription {
    provider: std::sync::Arc<dyn TranscriptionProvider>,
    /// Outstanding `StartTranscription` references.
    pub(super) refs: usize,
    /// Cancels the PCM pump tasks (and, via drop, detaches the leg taps).
    cancel: CancellationToken,
}

/// Forward one leg's decoded PCM to the provider until cancelled or closed.
/// Dropping `stream` on exit detaches the leg's decode task.
async fn pump_side(
    mut stream: crate::media::app_ingress::LegPcmStream,
    provider: std::sync::Arc<dyn TranscriptionProvider>,
    side: TranscriptSide,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            frame = stream.recv() => {
                match frame {
                    Some(f) if !f.silence => {
                        if provider.push_pcm(SidePcmFrame { side, frame: f.frame }).is_err() {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
}

impl SipSession {
    /// Attach per-leg PCM taps and start the provider. Returns the sides
    /// that actually carry a negotiated media leg.
    pub(super) async fn start_live_transcription(
        &mut self,
        language: Option<String>,
    ) -> Result<Vec<TranscriptSide>> {
        let remote = self
            .server
            .proxy_config
            .load()
            .transcript
            .as_ref()
            .and_then(|t| t.remote.clone())
            .ok_or_else(|| {
                anyhow!("live transcription not configured ([proxy.transcript.remote])")
            })?;
        if !remote.is_runnable() {
            return Err(anyhow!(
                "live transcription missing api_key (config or DEEPGRAM_API_KEY env)"
            ));
        }
        let mut remote = remote;
        if let Some(language) = language {
            remote.language = Some(language);
        }

        let bridge = self
            .bridge()
            .ok_or_else(|| anyhow!("transcription requires MediaBridge"))?;

        // Attach both legs; tolerate a missing side (single-leg calls).
        let mut sides = Vec::new();
        let mut stream_a = None;
        let mut stream_b = None;
        for leg_side in [LegSide::A, LegSide::B] {
            match bridge.leg_pcm_stream(leg_side) {
                Ok(stream) => {
                    sides.push(TranscriptSide::from_leg_side(leg_side));
                    match leg_side {
                        LegSide::A => stream_a = Some(stream),
                        LegSide::B => stream_b = Some(stream),
                    }
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
            return Err(anyhow!("no leg with negotiated media for transcription"));
        }

        let call_id = self.context.session_id.clone();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<TranscriptionEvent>();
        let provider: std::sync::Arc<dyn TranscriptionProvider> =
            std::sync::Arc::new(RemoteStreamingProvider::new(remote, &sides, event_tx));

        // PCM pump: one task per side, forwarding non-silence frames.
        let cancel = CancellationToken::new();
        {
            let provider = provider.clone();
            let pump_cancel = cancel.clone();
            if let Some(stream) = stream_a {
                let provider = provider.clone();
                let pump_cancel = pump_cancel.clone();
                tokio::spawn(async move {
                    pump_side(stream, provider, TranscriptSide::Caller, pump_cancel).await;
                });
            }
            if let Some(stream) = stream_b {
                let provider = provider.clone();
                let pump_cancel = pump_cancel.clone();
                tokio::spawn(async move {
                    pump_side(stream, provider, TranscriptSide::Callee, pump_cancel).await;
                });
            }
        }

        // Event forwarder: provider events → gateway transcript events.
        {
            let gateway = self.server.rwi_gateway.clone();
            let call_id = call_id.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    let Some(gateway) = gateway.as_ref() else {
                        break;
                    };
                    match event {
                        TranscriptionEvent::Segment(seg) => {
                            gateway.read().send_to_owner(&TranscriptSegmentEvent {
                                call_id: call_id.clone(),
                                side: seg.side.as_str().to_string(),
                                text: seg.text,
                                partial: seg.partial,
                                start_ms: seg.start_ms,
                                end_ms: seg.end_ms,
                                lang: seg.lang,
                            });
                        }
                        TranscriptionEvent::Failed { side, error } => {
                            tracing::warn!(
                                call_id = %call_id,
                                side = side.map(|s| s.as_str().to_string()).unwrap_or_default(),
                                error = %error,
                                "live transcription provider failure"
                            );
                            gateway.read().send_to_owner(&TranscriptError {
                                call_id: call_id.clone(),
                                side: side.map(|s| s.as_str().to_string()),
                                error,
                            });
                        }
                    }
                }
            });
        }

        self.live_transcription = Some(LiveTranscription {
            provider,
            refs: 1,
            cancel,
        });

        self.emit_typed_rwi_event(&TranscriptStarted {
            call_id,
            sides: sides.iter().map(|s| s.as_str().to_string()).collect(),
            provider: Some("deepgram".to_string()),
        });
        Ok(sides)
    }

    /// Stop the transcription pump unconditionally and emit
    /// `transcript_ended`. No-op when not running.
    pub(super) async fn stop_live_transcription(&mut self, reason: &str) {
        if let Some(lt) = self.live_transcription.take() {
            lt.cancel.cancel();
            lt.provider.stop().await;
            self.emit_typed_rwi_event(&TranscriptEnded {
                call_id: self.context.session_id.clone(),
                reason: reason.to_string(),
            });
        }
    }
}
