//! Generic per-leg ingress audio analysis tap (shared decode pipeline).
//!
//! There is at most ONE tap per leg: the RTP observer + decode task are only
//! started when at least one analyser actually needs ingress audio (in-band
//! DTMF, AMD detection) and dropped again when the last sink goes away, so
//! legs that never ask for audio analysis pay nothing.
//!
//! Pipeline: RTP observer (ingress, audio payload types only) → codec decode
//! → fixed 20 ms blocks → [`PcmBlockSink`] fan-out via a shared sink registry
//! (one slot per analyser kind, addable/removable while the task runs). A
//! codec change rebuilds the whole tap — the caller recreates the sinks and
//! decides per analysis whether detector state is carried across (AMD) or
//! restarted (DTMF).
//!
//! Starved ticks feed digital silence so gap-counting state machines (DTMF
//! re-arm, AMD pause tracking) keep advancing while the peer is silent; an
//! all-zero block can never classify as tone or speech.
//!
//! Lifecycle mirrors [`crate::app_ingress::LegPcmStream`]: dropping the tap
//! synchronously deactivates the packet observer and cancels the decode task
//! (rustrtc has no observer-removal API).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use audio_codec::{create_decoder, CodecType};
use parking_lot::Mutex;
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::ingress_tap::PtMask;
use crate::leg_id::LegId;
use crate::negotiate::NegotiatedLegProfile;

/// Analysis block length fed by the tap's decode task. Matches the DTMF
/// detector's block and gives AMD ~20 ms timing granularity.
pub const ANALYSIS_BLOCK_MS: u64 = 20;

/// Consumer of decoded ingress PCM blocks. Exactly one block per call with
/// `block_samples = sample_rate × ANALYSIS_BLOCK_MS / 1000` samples. The
/// sample rate is passed per block so a sink carried across a codec change
/// (re-INVITE) can adapt.
pub trait PcmBlockSink: Send {
    fn feed_block(&mut self, samples: &[i16], sample_rate: u32);
}

/// Identity of one analyser slot in the shared registry (one sink per kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisSinkKind {
    InbandDtmf,
    Amd,
}

/// Shared sink registry — the decode task iterates this every 20 ms block.
/// Add/remove happens outside the task (arm/disarm), so the lock is
/// uncontended on the hot path.
#[derive(Default)]
struct SinkRegistry {
    entries: Vec<(AnalysisSinkKind, Box<dyn PcmBlockSink>)>,
}

impl SinkRegistry {
    fn add(&mut self, kind: AnalysisSinkKind, sink: Box<dyn PcmBlockSink>) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == kind) {
            slot.1 = sink; // re-arm replaces the previous sink of this kind
        } else {
            self.entries.push((kind, sink));
        }
    }

    fn remove(&mut self, kind: AnalysisSinkKind) {
        self.entries.retain(|(k, _)| *k != kind);
    }
}

/// Forwards ingress audio-PT RTP packets into the decode task's channel.
/// telephone-event packets are excluded (decoding them yields garbage).
struct AudioPacketForwarder {
    audio_pts: PtMask,
    tx: mpsc::Sender<RtpPacket>,
    /// Set to `false` on tap drop: rustrtc has no observer-removal API, so
    /// deactivation prevents observer accumulation on repeated re-INVITEs.
    active: Arc<AtomicBool>,
}

impl RtpObserver for AudioPacketForwarder {
    fn on_ingress(&self, packet: &RtpPacket, _src_addr: std::net::SocketAddr) {
        if self.active.load(Ordering::Acquire) && self.audio_pts.contains(packet.header.payload_type)
        {
            let _ = self.tx.try_send(packet.clone());
        }
    }
    // Egress is not an input signal (mirrors the RFC 2833 tap semantics).
}

/// The leg's single analysis tap: decodes the leg's ingress audio RTP to PCM
/// and feeds fixed 20 ms blocks to every registered [`PcmBlockSink`].
pub struct PcmAnalysisTap {
    sinks: Arc<Mutex<SinkRegistry>>,
    cancel: CancellationToken,
    observer_active: Arc<AtomicBool>,
}

impl PcmAnalysisTap {
    /// Attach to `pc` using the leg's negotiated profile. Requires a
    /// negotiated audio codec (the analysers all work on decoded audio).
    /// Callers create the tap only when at least one sink is wanted.
    pub fn attach(
        pc: &rustrtc::PeerConnection,
        profile: &NegotiatedLegProfile,
        leg_id: LegId,
        sinks: Vec<(AnalysisSinkKind, Box<dyn PcmBlockSink>)>,
    ) -> Result<Self> {
        let audio = profile
            .audio
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("audio analysis tap requires a negotiated audio codec"))?;
        let codec = audio.codec;
        let sample_rate = codec.samplerate();
        let block_samples = ((u64::from(sample_rate) * ANALYSIS_BLOCK_MS) / 1000) as usize;

        // Only the negotiated audio PT is decoded; telephone-event PTs would
        // decode to noise (and RFC 2833 has its own detector).
        let audio_pts = PtMask::new();
        audio_pts.set(&[audio.payload_type]);

        let (pkt_tx, pkt_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();
        let observer_active = Arc::new(AtomicBool::new(true));
        let registry = Arc::new(Mutex::new(SinkRegistry {
            entries: sinks,
        }));

        pc.add_observer(Arc::new(AudioPacketForwarder {
            audio_pts,
            tx: pkt_tx,
            active: observer_active.clone(),
        }));

        tokio::spawn(analysis_task(
            pkt_rx,
            Arc::clone(&registry),
            codec,
            sample_rate,
            block_samples,
            leg_id,
            cancel.clone(),
        ));

        Ok(Self {
            sinks: registry,
            cancel,
            observer_active,
        })
    }

    /// Register (or replace) the sink for `kind` on the running tap. Takes a
    /// full registry entry (what the `make_sink` constructors return).
    pub fn add_sink(&self, entry: (AnalysisSinkKind, Box<dyn PcmBlockSink>)) {
        let (kind, sink) = entry;
        self.sinks.lock().add(kind, sink);
    }

    /// Remove the sink for `kind`. Returns `true` when the registry became
    /// empty — the caller should then drop the tap to stop the observer and
    /// decode task.
    pub fn remove_sink(&self, kind: AnalysisSinkKind) -> bool {
        let mut registry = self.sinks.lock();
        registry.remove(kind);
        registry.entries.is_empty()
    }

    /// Whether any sink is still registered.
    pub fn is_empty(&self) -> bool {
        self.sinks.lock().entries.is_empty()
    }
}

impl Drop for PcmAnalysisTap {
    fn drop(&mut self) {
        // Deactivate the observer BEFORE cancelling so no further packets are
        // forwarded (rustrtc has no observer removal API).
        self.observer_active.store(false, Ordering::Release);
        self.cancel.cancel();
    }
}

/// Real-time decode + analyse loop: one sink block per 20 ms tick. Starved
/// ticks feed digital silence so gap counters keep advancing (a stopped audio
/// stream must eventually register as silence). Decoded backlog is drained
/// fully each tick to bound detection latency.
async fn analysis_task(
    mut pkt_rx: mpsc::Receiver<RtpPacket>,
    sinks: Arc<Mutex<SinkRegistry>>,
    codec: CodecType,
    sample_rate: u32,
    block_samples: usize,
    leg: LegId,
    cancel: CancellationToken,
) {
    let mut decoder = create_decoder(codec);
    let mut buffer: Vec<i16> = Vec::with_capacity(block_samples * 4);
    let silence = vec![0i16; block_samples];
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(ANALYSIS_BLOCK_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            pkt = pkt_rx.recv() => match pkt {
                Some(p) => {
                    let pcm = decoder.decode(&p.payload);
                    buffer.extend_from_slice(&pcm);
                }
                None => break,
            },
            _ = interval.tick() => {
                let mut registry = sinks.lock();
                let mut fed = false;
                while buffer.len() >= block_samples {
                    let block: Vec<i16> = buffer.drain(..block_samples).collect();
                    fed = true;
                    for (_, sink) in registry.entries.iter_mut() {
                        sink.feed_block(&block, sample_rate);
                    }
                }
                if !fed && buffer.is_empty() {
                    // No audio this tick (peer silent / pre-answer): advance
                    // the gap state; an all-zero block never classifies.
                    for (_, sink) in registry.entries.iter_mut() {
                        sink.feed_block(&silence, sample_rate);
                    }
                }
            }
        }
    }
    debug!(leg = %leg, codec = ?codec, "pcm analysis task stopped");
}
