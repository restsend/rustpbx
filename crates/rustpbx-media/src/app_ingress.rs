//! Single-leg decoded PCM ingress.
//!
//! [`LegPcmStream`] attaches a dedicated decode task to one leg's incoming
//! RTP: a packet-forwarding observer feeds every ingress packet into a
//! channel, and the decode task emits one PCM frame per ptime tick. Silence
//! frames keep the cadence when the leg is starved, so downstream consumers
//! (conference / supervisor mixer input, live transcription) always see a
//! continuous frame stream.
//!
//! Dropping the stream cancels the decode task and deactivates the
//! forwarding observer, so consumers that start/stop the tap repeatedly
//! (live transcription, supervisor) never accumulate observers on the
//! PeerConnection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use audio_codec::{CodecType, create_decoder};
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::AudioFrame;
use crate::leg_id::LegId;
use crate::negotiate::NegotiatedLegProfile;

const DEFAULT_PTIME_MS: u64 = 20;

/// A decoded PCM frame.
#[derive(Debug, Clone)]
pub struct PcmFrame {
    pub frame: AudioFrame,
    /// `true` when this frame is silence (no buffered data this tick). Lets
    /// consumers skip processing or mark gaps.
    pub silence: bool,
}

/// Single-leg decoded PCM stream.
///
/// Attach a leg's `PeerConnection` + negotiated profile, then `recv()`
/// returns one PCM frame per ptime tick (silence frames keep cadence when
/// the leg is starved).
pub struct LegPcmStream {
    rx: mpsc::Receiver<PcmFrame>,
    cancel: CancellationToken,
    /// Shared with the installed `PacketForwarder` observer; set to `false`
    /// on drop so the observer stops forwarding packets even though rustrtc
    /// does not expose a "remove observer" API. Prevents observer
    /// accumulation from pushing into a dead channel forever.
    observer_active: Arc<AtomicBool>,
}

impl LegPcmStream {
    /// Attach to `pc` (the leg's PeerConnection) using its negotiated
    /// profile. Installs a packet-forwarding observer that decodes ingress
    /// RTP to PCM. `parent_token` (e.g. the MediaBridge root token) cancels
    /// the decode task independently of this stream's lifetime.
    pub fn attach(
        pc: &rustrtc::PeerConnection,
        profile: NegotiatedLegProfile,
        leg_id: LegId,
        parent_token: CancellationToken,
    ) -> Result<Self> {
        let audio = profile
            .audio
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no negotiated audio codec for leg {leg_id}"))?;
        let codec = audio.codec;
        let sample_rate = audio.codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(256);
        let (frame_tx, frame_rx) = mpsc::channel::<PcmFrame>(16);
        let cancel = parent_token.child_token();
        let observer_active = Arc::new(AtomicBool::new(true));

        // Install a forwarding observer on the PC (fires on every ingress
        // plaintext packet, including relay fast-path).
        pc.add_observer(Arc::new(PacketForwarder {
            tx: pkt_tx,
            active: observer_active.clone(),
        }));

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            leg_id,
            codec,
            sample_rate,
            pcm_per_frame,
            cancel.clone(),
        ));

        Ok(Self {
            rx: frame_rx,
            cancel,
            observer_active,
        })
    }

    /// Receive the next PCM frame. Returns `None` when the stream is closed
    /// (decode task stopped).
    pub async fn recv(&mut self) -> Option<PcmFrame> {
        self.rx.recv().await
    }
}

impl Drop for LegPcmStream {
    fn drop(&mut self) {
        // Deactivate the observer BEFORE cancelling so the observer stops
        // forwarding immediately (rustrtc has no observer removal API).
        self.observer_active.store(false, Ordering::Release);
        self.cancel.cancel();
    }
}

/// Forwards ingress RTP packets into a channel for the decode task.
struct PacketForwarder {
    tx: mpsc::Sender<RtpPacket>,
    /// Set to `false` on detach so the observer stops forwarding.
    active: Arc<AtomicBool>,
}

impl RtpObserver for PacketForwarder {
    fn on_ingress(&self, packet: &RtpPacket, _src_addr: std::net::SocketAddr) {
        if self.active.load(Ordering::Acquire) {
            let _ = self.tx.try_send(packet.clone());
        }
    }
    // Egress not needed for ingress aggregation.
}

/// Decode-task: drains packets, decodes to PCM, emits one PCM frame per ptime.
///
/// When the consumer is slow the frame channel (capacity 16) drops frames
/// (`try_send`) rather than letting the decoder fall behind real time —
/// matching the previous broadcast lagged-drop behavior.
async fn decode_task(
    mut pkt_rx: mpsc::Receiver<RtpPacket>,
    frame_tx: mpsc::Sender<PcmFrame>,
    leg: LegId,
    codec: CodecType,
    sample_rate: u32,
    pcm_per_frame: usize,
    cancel: CancellationToken,
) {
    let mut decoder = create_decoder(codec);
    let mut buffer: Vec<i16> = Vec::with_capacity(pcm_per_frame * 4);
    let mut interval = tokio::time::interval(Duration::from_millis(DEFAULT_PTIME_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            pkt = pkt_rx.recv() => match pkt {
                Some(p) => {
                    // Telephone-event payloads would decode to garbage; skip
                    // non-audio by length heuristics is unreliable, so we rely
                    // on the negotiated codec: only audio PTs reach here in
                    // practice (DTMF is filtered upstream).
                    let pcm = decoder.decode(&p.payload);
                    buffer.extend_from_slice(&pcm);
                }
                None => break,
            },
            _ = interval.tick() => {
                let (samples, silence) = if buffer.len() >= pcm_per_frame {
                    let frame: Vec<i16> = buffer.drain(..pcm_per_frame).collect();
                    (frame, false)
                } else {
                    // Not enough decoded PCM yet → pad with silence to keep
                    // cadence (buffer continues accumulating).
                    (vec![0i16; pcm_per_frame], true)
                };
                let _ = frame_tx.try_send(PcmFrame {
                    frame: AudioFrame::new(samples, sample_rate),
                    silence,
                });
            }
        }
    }
    trace!(leg = %leg, "app-ingress decode task stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::rtp::{RtpHeader, RtpPacket};

    /// Drive the decode task directly (no PeerConnection) and verify it emits
    /// silence frames when starved, and PCM frames when fed.
    #[tokio::test]
    async fn decode_task_emits_silence_then_pcm() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();

        // PCMU @ 8kHz, 20ms → 160 samples/frame.
        let codec = CodecType::PCMU;
        let sample_rate = codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("a"),
            codec,
            sample_rate,
            pcm_per_frame,
            cancel.clone(),
        ));

        // First tick(s) with no data → silence frames.
        let f1 = rx.recv().await.unwrap();
        assert!(f1.silence, "starved task must emit silence");
        assert_eq!(f1.frame.samples.len(), pcm_per_frame);

        // Feed enough PCMU data to fill a frame. PCMU decode of N bytes → N samples.
        // Feed 3 frames worth of payload so at least one non-silence frame lands.
        for i in 0..3 {
            let p = RtpPacket::new(RtpHeader::new(0, i, (i as u32) * 160, 1), vec![0xFFu8; 160]);
            let _ = pkt_tx.try_send(p);
        }
        // Allow the task to decode + a couple ticks.
        let mut saw_pcm = false;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(f)) => {
                    if !f.silence {
                        saw_pcm = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            saw_pcm,
            "must emit at least one non-silence PCM frame after feeding data"
        );

        cancel.cancel();
    }

    /// A slow consumer drops frames (bounded channel) but the stream stays
    /// open — `recv` keeps returning after the channel has been full.
    #[tokio::test]
    async fn slow_consumer_does_not_close_stream() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(2);
        let (_pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let codec = CodecType::PCMU;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("s"),
            codec,
            sr,
            pf,
            cancel.clone(),
        ));

        // Don't read for a while → channel overflows and frames drop...
        tokio::time::sleep(Duration::from_millis(150)).await;
        // ...but recv still works afterwards (task alive, cadence preserved).
        let f = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("recv after overflow")
            .expect("stream still open");
        assert_eq!(f.frame.samples.len(), pf);

        cancel.cancel();
    }
}
