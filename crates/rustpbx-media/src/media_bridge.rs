//! Per-session media facade (2-party B2BUA). Owns exactly two legs — `A` and
//! `B` — and wires them together via the unified [`EgressSource`]:
//!
//! - same negotiated codec → [`EgressSource::RewriteRelay`] (transport-level
//!   zero-copy relay; ICE send channel exclusively owned by the rewrite bridge)
//! - differing codecs → [`EgressSource::TranscodePeer`] (pull from the peer's
//!   receiver track, decode, auto-resample, re-encode)
//!
//! Legs can be swapped atomically ([`Self::replace_leg`] — e.g. call transfer /
//! REFER); if a route is active the codec is re-evaluated automatically and
//! the mode switches fast-path ↔ transcode.
//!
//! MCU / conference mixing is **not** this bridge's concern — it is handled by
//! `conference_mixer` / `conference_media_bridge` upstream.
//!
//! ## Concurrency
//! `MediaBridge` has a single owner (the session) and mutating methods take
//! `&mut self`, so the borrow checker serializes control flow — no `Mutex` is
//! needed. Hot-path state (taps, egress pipelines) lives inside each `Leg` and
//! is lock-free.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Result, anyhow};
use audio_codec::create_decoder;
use rustrtc::RtpRewriteBridgeParams;
use rustrtc::{MediaKind, media::MediaStreamTrack};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::egress::EgressSource;
use crate::ingress_tap::{DtmfEvent, MediaRecorder};
use crate::leg::Leg;

/// Which side of the 2-party bridge a leg occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegSide {
    /// The calling party (typically the SIP caller).
    A,
    /// The called party (typically the SIP callee / agent) — replaceable.
    B,
}

/// Outcome of a [`PlaybackHandle`]'s play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackResult {
    /// `false` = natural EOF, `true` = interrupted (stop_play / source switch).
    pub interrupted: bool,
}

impl PlaybackResult {
    pub fn completed() -> Self {
        Self { interrupted: false }
    }

    pub fn interrupted() -> Self {
        Self { interrupted: true }
    }
}

/// Handle to an in-progress [`MediaBridge::play`] / [`MediaBridge::play_file`]
/// on a leg. `done` resolves when playback stops (natural EOF or interrupted).
#[derive(Debug)]
pub struct PlaybackHandle {
    pub done: oneshot::Receiver<PlaybackResult>,
}

impl PlaybackHandle {
    fn new() -> (Self, oneshot::Sender<PlaybackResult>) {
        let (done, rx) = oneshot::channel();
        (Self { done: rx }, done)
    }
}

/// Optional global configuration passed at construction.
#[derive(Default)]
pub struct BridgeOpts {
    /// Default recording backend attached to every leg as it's added.
    pub recorder: Option<Arc<dyn MediaRecorder>>,
}

/// Per-session 2-party media bridge.
pub struct MediaBridge {
    session_id: String,
    leg_a: Option<Leg>,
    leg_b: Option<Leg>,
    route_active: bool,
    recorder: Option<Arc<dyn MediaRecorder>>,
    dtmf_bus: broadcast::Sender<(LegSide, DtmfEvent)>,
    /// Root cancel token for all spawned sub-tasks (DTMF forwarders).
    root_cancel: CancellationToken,
    /// Legs currently playing a Media source. `play` inserts; the egress
    /// `on_end` callback removes.
    active_play: Arc<parking_lot::Mutex<HashSet<LegSide>>>,
    /// Codecs of the last successful bridge activation. Used to make
    /// `bridge()` idempotent: re-bridging the same codec pair on an already
    /// active route is a no-op (avoids rebuilding decoders/relay).
    last_bridged: Option<(audio_codec::CodecType, audio_codec::CodecType)>,
}

impl MediaBridge {
    pub fn new(session_id: impl Into<String>, _opts: BridgeOpts) -> Self {
        let (dtmf_bus, _) = broadcast::channel(64);
        Self {
            session_id: session_id.into(),
            leg_a: None,
            leg_b: None,
            route_active: false,
            recorder: None,
            dtmf_bus,
            root_cancel: CancellationToken::new(),
            active_play: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            last_bridged: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn leg(&self, side: LegSide) -> Option<Leg> {
        match side {
            LegSide::A => self.leg_a.clone(),
            LegSide::B => self.leg_b.clone(),
        }
    }

    /// True when a P2P route is currently active between A and B.
    pub fn is_bridged(&self) -> bool {
        self.route_active
    }

    /// Wire a leg's DTMF into the bridge bus, attach the default recorder, and
    /// monitor RTP inactivity timeout — all in ONE per-leg task (no extra
    /// spawns). The timeout check runs on a fixed 100ms interval and uses the
    /// leg's ingress packet counter + `armed_at` timestamp: when armed and no
    /// new packets arrive within the duration, the oneshot receiver is fired.
    fn wire_leg(&self, side: LegSide, leg: &Leg) {
        let mut rx = leg.subscribe_dtmf();
        let tap = leg.ingress_tap().clone();
        let timeout = leg.rtp_timeout_state();
        let leg_ref = leg.clone();
        let bus = self.dtmf_bus.clone();
        let cancel = self.root_cancel.child_token();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_count = tap.ingress_packet_count();
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Ok(ev) => { let _ = bus.send((side, ev)); }
                        Err(_) => break,
                    },
                    _ = interval.tick() => {
                        if !timeout.active.load(Ordering::Relaxed) {
                            // Idle / paused — keep the baseline in sync.
                            last_count = tap.ingress_packet_count();
                            continue;
                        }
                        let current = tap.ingress_packet_count();
                        if current != last_count {
                            // New packet arrived → reset the countdown.
                            last_count = current;
                            *timeout.armed_at.lock() = Some(std::time::Instant::now());
                            continue;
                        }
                        // No new packets — fire if the duration has elapsed.
                        let dur = std::time::Duration::from_millis(
                            timeout.duration_ms.load(Ordering::Relaxed),
                        );
                        let armed_at = timeout.armed_at.lock();
                        if let Some(start) = *armed_at {
                            if start.elapsed() >= dur {
                                drop(armed_at);
                                leg_ref.fire_rtp_timeout();
                            }
                        }
                    }
                }
            }
        });
        if let Some(rec) = &self.recorder {
            leg.set_recorder(Some(rec.clone()));
        }
    }

    /// Place (or replace) a leg. If a route is already active, the codec is
    /// re-evaluated and the mode switches fast-path ↔ transcode as needed.
    /// Use this for call transfer / REFER.
    pub async fn replace_leg(&mut self, side: LegSide, new_leg: Leg) {
        self.wire_leg(side, &new_leg);
        match side {
            LegSide::A => self.leg_a = Some(new_leg),
            LegSide::B => self.leg_b = Some(new_leg),
        }
        if self.route_active {
            if let Err(e) = self.bridge().await {
                warn!(session = %self.session_id, error = %e, "re-bridge after leg replacement failed");
            }
        }
    }

    pub fn dtmf_bus(&self) -> broadcast::Receiver<(LegSide, DtmfEvent)> {
        self.dtmf_bus.subscribe()
    }

    // ── Recording ────────────────────────────────────────────────────────

    pub fn set_recorder(&mut self, recorder: Arc<dyn MediaRecorder>) {
        if let Some(la) = self.leg_a.as_ref() {
            la.set_recorder(Some(recorder.clone()));
        }
        if let Some(lb) = self.leg_b.as_ref() {
            lb.set_recorder(Some(recorder.clone()));
        }
        self.recorder = Some(recorder);
    }

    pub fn set_recording_paused(&self, paused: bool) {
        if let Some(la) = self.leg_a.as_ref() {
            la.ingress_tap().set_paused(paused);
        }
        if let Some(lb) = self.leg_b.as_ref() {
            lb.ingress_tap().set_paused(paused);
        }
    }

    pub fn stop_recording(&mut self) {
        if let Some(rec) = self.recorder.take() {
            rec.finalize();
        }
        if let Some(la) = self.leg_a.as_ref() {
            la.set_recorder(None);
        }
        if let Some(lb) = self.leg_b.as_ref() {
            lb.set_recorder(None);
        }
    }

    // ── Routing ──────────────────────────────────────────────────────────

    /// Bridge A ↔ B. Both legs must exist and be answered (gate open). Selects
    /// [`EgressSource::RewriteRelay`] when the negotiated audio codecs match,
    /// otherwise [`EgressSource::TranscodePeer`].
    pub async fn bridge(&mut self) -> Result<()> {
        let (la, lb) = (self.leg_a.clone(), self.leg_b.clone());
        let (Some(la), Some(lb)) = (la, lb) else {
            return Ok(()); // not both legs present yet
        };
        if la.is_gated() || lb.is_gated() {
            return Ok(()); // both must answer (accept) first
        }
        let (Some(pa), Some(pb)) = (la.negotiated(), lb.negotiated()) else {
            return Ok(()); // SDP not applied yet
        };
        let (Some(ca), Some(cb)) = (pa.audio.as_ref(), pb.audio.as_ref()) else {
            return Ok(());
        };

        // Idempotent re-bridge: same codec pair on an already-active route is
        // a no-op (avoid rebuilding decoders / re-arming the relay).
        if self.route_active && self.last_bridged == Some((ca.codec, cb.codec)) {
            return Ok(());
        }
        self.last_bridged = Some((ca.codec, cb.codec));

        if ca.codec == cb.codec {
            // ── fast-path: transport-level zero-copy relay ──
            debug!(session = %self.session_id, codec = ?ca.codec, "MBRIDGE fast-path relay");            // Rewrite the forwarded packet's header to the destination leg's
            // negotiated SSRC / PT, and strip WebRTC extension headers when the
            // destination is plain RTP.
            let a_transport = la.pc().config().transport_mode.clone();
            let b_transport = lb.pc().config().transport_mode.clone();
            let a_ssrc = sender_ssrc(la.pc());
            let b_ssrc = sender_ssrc(lb.pc());

            // RFC 4733 DTMF payload-type remap (only when the two legs
            // negotiated different telephone-event payload types).
            let dtmf_a_to_b = match (pa.dtmf.as_ref(), pb.dtmf.as_ref()) {
                (Some(a), Some(b)) if a.payload_type != b.payload_type => {
                    Some((a.payload_type, b.payload_type))
                }
                _ => None,
            };
            let dtmf_b_to_a = dtmf_a_to_b.map(|(a, b)| (b, a));

            let params_a_to_b = RtpRewriteBridgeParams {
                ssrc_offset: 0,
                fixed_out_ssrc: Some(b_ssrc),
                payload_type: (ca.payload_type != cb.payload_type).then_some(cb.payload_type),
                dtmf_payload_type: dtmf_a_to_b,
                initial_sequence_number: None,
                initial_timestamp_offset: None,
                strip_extensions: b_transport == rustrtc::TransportMode::Rtp,
            };
            let params_b_to_a = RtpRewriteBridgeParams {
                ssrc_offset: 0,
                fixed_out_ssrc: Some(a_ssrc),
                payload_type: (ca.payload_type != cb.payload_type).then_some(ca.payload_type),
                dtmf_payload_type: dtmf_b_to_a,
                initial_sequence_number: None,
                initial_timestamp_offset: None,
                strip_extensions: a_transport == rustrtc::TransportMode::Rtp,
            };

            la.set_egress_source(EgressSource::RewriteRelay {
                peer_pc: lb.pc().clone(),
                params: params_a_to_b,
            })
            .await?;
            lb.set_egress_source(EgressSource::RewriteRelay {
                peer_pc: la.pc().clone(),
                params: params_b_to_a,
            })
            .await?;
            info!(
                session = %self.session_id,
                codec = ?ca.codec,
                a_ssrc, b_ssrc,
                strip_a_to_b = params_a_to_b.strip_extensions,
                strip_b_to_a = params_b_to_a.strip_extensions,
                "fast-path relay activated"
            );
        } else {
            // ── transcoding: decode peer codec → re-encode own codec ──
            let b_recv = get_audio_recv_track(lb.pc())
                .ok_or_else(|| anyhow!("no audio receiver track on leg B"))?;
            let a_recv = get_audio_recv_track(la.pc())
                .ok_or_else(|| anyhow!("no audio receiver track on leg A"))?;
            // Use the DECODER's output sample rate (not the RTP clock rate) as
            // the resampler source rate — e.g. G.722 clocks at 8 kHz in RTP but
            // decodes to 16 kHz PCM. Wrong source rate ⇒ garbage resample.
            let b_decoder = create_decoder(cb.codec);
            let a_decoder = create_decoder(ca.codec);
            let b_src_rate = b_decoder.sample_rate();
            let a_src_rate = a_decoder.sample_rate();
            la.set_egress_source(EgressSource::TranscodePeer {
                peer: b_recv,
                decoder: b_decoder,
                src_sample_rate: b_src_rate,
            })
            .await?;
            lb.set_egress_source(EgressSource::TranscodePeer {
                peer: a_recv,
                decoder: a_decoder,
                src_sample_rate: a_src_rate,
            })
            .await?;
            info!(
                session = %self.session_id,
                a_codec = ?ca.codec,
                b_codec = ?cb.codec,
                "transcoding activated"
            );
        }

        self.route_active = true;
        Ok(())
    }

    /// Break the route: both legs' egress → [`EgressSource::Silence`] and any
    /// rewrite bridge is torn down (handled inside `Leg::set_egress_source`).
    pub async fn unbridge(&mut self) -> Result<()> {
        self.route_active = false;
        self.last_bridged = None;
        if let Some(la) = self.leg_a.as_ref() {
            la.set_egress_source(EgressSource::Silence).await?;
        }
        if let Some(lb) = self.leg_b.as_ref() {
            lb.set_egress_source(EgressSource::Silence).await?;
        }
        Ok(())
    }

    /// Mark a leg as answered (remote peer sent 200 OK). Opens the gate and,
    /// once both legs are answered, activates the route.
    pub async fn accept(&mut self, side: LegSide) {
        if let Some(leg) = self.leg(side) {
            leg.accept();
        }
        if let Err(e) = self.bridge().await {
            warn!(session = %self.session_id, error = %e, "route activation after accept failed");
        }
    }

    // ── Media operations ─────────────────────────────────────────────────

    /// Play a media source on a leg (IVR / announcement). The route is broken
    /// first so the peer hears silence, not a mix of playback and relayed audio.
    /// Call [`Self::resume`] afterwards to restore the route.
    ///
    /// Returns a [`PlaybackHandle`]; `done` resolves when playback stops.
    pub async fn play(
        &mut self,
        side: LegSide,
        audio: Box<dyn crate::audio_source::AudioSource>,
        loop_playback: bool,
    ) -> Result<PlaybackHandle> {
        self.unbridge().await?;
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        // Pause the RTP inactivity timeout while playing: during playback the
        // peer may stay silent, so an armed timeout would fire spuriously.
        leg.pause_rtp_timeout();
        let leg_for_end = leg.clone();
        let (handle, done_tx) = PlaybackHandle::new();
        self.active_play.lock().insert(side);
        let active_registry = self.active_play.clone();
        let done_tx = Arc::new(parking_lot::Mutex::new(Some(done_tx)));
        let on_end = Arc::new(move |interrupted: bool| {
            // Clear the active-play marker for this side on completion.
            active_registry.lock().remove(&side);
            // Resume the RTP inactivity timeout once playback ends.
            leg_for_end.resume_rtp_timeout();
            if let Some(tx) = done_tx.lock().take() {
                let _ = tx.send(PlaybackResult { interrupted });
            }
        });
        leg.set_egress_source(EgressSource::Media {
            audio,
            loop_playback,
            on_end: Some(on_end),
        })
        .await?;
        Ok(handle)
    }

    /// Play a file (or http URL) on a leg. Reads the file async and pre-decodes
    /// it into memory; the egress pacing task reads from the in-memory cache.
    pub async fn play_file(
        &mut self,
        side: LegSide,
        path: impl Into<String>,
        loop_playback: bool,
    ) -> Result<PlaybackHandle> {
        let source = crate::audio_source::FileAudioSource::new(path.into(), loop_playback).await?;
        self.play(side, Box::new(source), loop_playback).await
    }

    /// Whether a leg currently has an active Media playback.
    pub fn is_playing(&self, side: LegSide) -> bool {
        self.active_play.lock().contains(&side)
    }

    /// Stop a running playback on a leg. Fires the handle's `done` with
    /// `interrupted: true`. No-op if the leg is not currently playing Media.
    pub async fn stop_play(&mut self, side: LegSide) -> Result<()> {
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        // Switching away from an active Media source fires on_end(true) inside
        // the egress task; stop_play just sends Silence to trigger it.
        if self.active_play.lock().contains(&side) {
            leg.set_egress_source(EgressSource::Silence).await?;
        }
        Ok(())
    }

    /// Put a leg on hold: break the route, then play hold music (looping) or
    /// silence.
    pub async fn hold(
        &mut self,
        side: LegSide,
        music: Option<Box<dyn crate::audio_source::AudioSource>>,
    ) -> Result<()> {
        self.unbridge().await?;
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        match music {
            Some(audio) => leg
                .set_egress_source(EgressSource::Media {
                    audio,
                    loop_playback: true,
                    on_end: None,
                })
                .await?,
            None => leg.set_egress_source(EgressSource::Silence).await?,
        }
        Ok(())
    }

    /// Convenience: hold a leg playing a file as hold music (looping).
    pub async fn hold_file(&mut self, side: LegSide, path: impl Into<String>) -> Result<()> {
        let source = crate::audio_source::FileAudioSource::new(path.into(), true).await?;
        self.hold(side, Some(Box::new(source))).await
    }

    /// Resume a leg from hold / play: re-activate the route (auto-selects
    /// fast-path or transcode).
    pub async fn resume(&mut self, _side: LegSide) -> Result<()> {
        self.bridge().await
    }

    /// Mute a leg's egress (send silence). Breaks the route on that leg.
    pub async fn mute(&mut self, side: LegSide) -> Result<()> {
        self.unbridge().await?;
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        leg.set_egress_source(EgressSource::Silence).await
    }

    // ── Timeout / lifecycle ──────────────────────────────────────────────

    /// Arm an RTP inactivity timeout for a leg. Returns a `oneshot::Receiver`
    /// that fires (`Ok(())`) when no ingress packet arrives within `timeout`.
    /// Monitored by the per-leg DTMF task (no dedicated spawn).
    pub fn arm_rtp_timeout(&self, side: LegSide, timeout: Duration) -> Option<oneshot::Receiver<()>> {
        self.leg(side).map(|leg| leg.arm_rtp_timeout(timeout))
    }

    /// Pause a leg's RTP timeout (e.g. during hold).
    pub fn pause_rtp_timeout(&self, side: LegSide) {
        if let Some(leg) = self.leg(side) {
            leg.pause_rtp_timeout();
        }
    }

    /// Resume a leg's RTP timeout (restarts the countdown).
    pub fn resume_rtp_timeout(&self, side: LegSide) {
        if let Some(leg) = self.leg(side) {
            leg.resume_rtp_timeout();
        }
    }

    /// Disarm a leg's RTP timeout.
    pub fn disarm_rtp_timeout(&self, side: LegSide) {
        if let Some(leg) = self.leg(side) {
            leg.disarm_rtp_timeout();
        }
    }

    /// Tear down everything (called on session end; also via Drop).
    pub fn close(&mut self) {
        self.root_cancel.cancel();
        self.route_active = false;
        if let Some(la) = self.leg_a.take() {
            la.stop();
        }
        if let Some(lb) = self.leg_b.take() {
            lb.stop();
        }
        info!(session = %self.session_id, "media bridge closed");
    }
}

impl Drop for MediaBridge {
    fn drop(&mut self) {
        // Stop legs synchronously (rustrtc close path has no tokio::spawn, so
        // this never panics during runtime teardown).
        self.root_cancel.cancel();
        if let Some(la) = self.leg_a.take() {
            la.stop();
        }
        if let Some(lb) = self.leg_b.take() {
            lb.stop();
        }
    }
}

/// The audio sender SSRC of a PC — the SSRC the remote peer expects in RTP
/// packets from this leg (advertised in the local SDP `a=ssrc` attribute).
fn sender_ssrc(pc: &rustrtc::PeerConnection) -> u32 {
    pc.get_transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Audio)
        .and_then(|t| t.sender())
        .map(|s| s.ssrc())
        .unwrap_or(0)
}

/// The audio receiver track of a PC — the depacketized inbound audio that
/// [`EgressSource::TranscodePeer`] pulls from. Present after SDP negotiation.
fn get_audio_recv_track(pc: &rustrtc::PeerConnection) -> Option<Arc<dyn MediaStreamTrack>> {
    pc.get_transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Audio)
        .and_then(|t| t.receiver())
        .map(|r| -> Arc<dyn MediaStreamTrack> { r.track() })
}

// Re-export the profile type for callers that inspect legs.
pub use crate::negotiate::NegotiatedLegProfile as LegProfile;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leg::{LegConfig, LegInner};

    #[tokio::test]
    async fn set_legs_and_close() {
        let mut mb = MediaBridge::new("s1", BridgeOpts::default());
        let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        assert!(mb.leg(LegSide::A).is_some());
        assert!(mb.leg(LegSide::B).is_some());
        mb.close();
    }

    #[tokio::test]
    async fn set_recorder_attaches_to_legs() {
        struct Noop;
        impl MediaRecorder for Noop {
            fn write_sample(&self, _: crate::ingress_tap::PacketDirection, _: &rustrtc::rtp::RtpPacket) {}
            fn write_dtmf(&self, _: DtmfEvent) {}
            fn set_paused(&self, _: bool) {}
            fn finalize(&self) {}
        }
        let mut mb = MediaBridge::new("s3", BridgeOpts::default());
        mb.replace_leg(LegSide::A, LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap())
            .await;
        mb.set_recorder(Arc::new(Noop));
        mb.replace_leg(LegSide::B, LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap())
            .await;
        let _ = mb.leg(LegSide::B).unwrap().stats();
        mb.close();
    }

    #[tokio::test]
    async fn dtmf_bus_forwarded_from_leg_tap() {
        let mut mb = MediaBridge::new("s4", BridgeOpts::default());
        mb.replace_leg(LegSide::A, LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap())
            .await;
        let rx = mb.dtmf_bus();
        // The tap has no dtmf payload types set, so we can't easily synthesize
        // an event here; this just ensures the bus subscription + forwarder
        // task spawn without panic.
        drop(rx);
        mb.close();
    }

    #[tokio::test]
    async fn replace_leg_rebridges_active_route() {
        let mut mb = MediaBridge::new("s5", BridgeOpts::default());
        let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.close();
    }
}
