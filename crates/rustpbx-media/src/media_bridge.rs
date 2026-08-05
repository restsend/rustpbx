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

impl LegSide {
    pub fn opposite(self) -> Self {
        match self {
            LegSide::A => LegSide::B,
            LegSide::B => LegSide::A,
        }
    }
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

    /// Return a decoded PCM stream for a leg's ingress RTP. The caller must
    /// have the leg's negotiated profile ready (i.e. SDP already applied).
    /// Used as the conference / supervisor mixer data source: each leg's PCM
    /// flows into the mixer instead of being read from an independent PC.
    pub fn leg_pcm_stream(
        &self,
        side: LegSide,
    ) -> Result<crate::app_ingress::LegPcmStream> {
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on side {:?}", side))?;
        let profile = leg
            .negotiated()
            .ok_or_else(|| anyhow!("leg on side {:?} has no negotiated profile", side))?;
        let leg_id = crate::leg_id::LegId::from(format!(
            "{}-{}",
            self.session_id,
            match side {
                LegSide::A => "a",
                LegSide::B => "b",
            }
        ));
        crate::app_ingress::LegPcmStream::attach(
            leg.pc(),
            profile,
            leg_id,
            self.root_cancel.child_token(),
        )
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
            // The leg instance changed (e.g. transfer swaps the B leg to a new
            // peer). Clear the idempotency cache so `bridge()` rebuilds the
            // relay even when the negotiated codec pair is unchanged — otherwise
            // the fast-path rewrite still targets the replaced leg's transport.
            self.last_bridged = None;
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

            // DTMF agreement check: a shared telephone-event PT is forwarded
            // raw (no remap possible), so the two legs must agree on its clock
            // rate too, otherwise relayed events carry the wrong timestamp /
            // duration interpretation. Some endpoints (e.g. SipBot's
            // telephone-event injection) answer telephone-event at a different
            // rate than the one offered; warn so the mismatch is visible.
            if let (Some(a), Some(b)) = (pa.dtmf.as_ref(), pb.dtmf.as_ref()) {
                if a.clock_rate != b.clock_rate {
                    warn!(
                        session = %self.session_id,
                        a_pt = a.payload_type,
                        a_clock = a.clock_rate,
                        b_pt = b.payload_type,
                        b_clock = b.clock_rate,
                        "DTMF telephone-event clock rates disagree between legs; relayed DTMF may be mistimed"
                    );
                }
            }
            for (side, leg) in [("a", &la), ("b", &lb)] {
                let profile = leg.negotiated();
                if let (Some(dtmf), Some(audio)) = (
                    profile.as_ref().and_then(|p| p.dtmf.as_ref()),
                    profile.as_ref().and_then(|p| p.audio.as_ref()),
                ) && dtmf.clock_rate != audio.clock_rate
                {
                    warn!(
                        session = %self.session_id,
                        side,
                        dtmf_pt = dtmf.payload_type,
                        dtmf_clock = dtmf.clock_rate,
                        audio_clock = audio.clock_rate,
                        "DTMF telephone-event clock rate does not match the leg audio codec clock (RFC 4733)"
                    );
                }
            }

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
        // The RTP transports may not be ready yet at accept time — a WebRTC
        // caller's DTLS/SRTP transport is only created after the 200 OK is
        // sent. Retry the route activation briefly instead of leaving the
        // relay un-armed (which would strand the call with no media until the
        // RTP inactivity timeout fires).
        const MAX_ACCEPT_RETRIES: usize = 5;
        for attempt in 0..=MAX_ACCEPT_RETRIES {
            match self.bridge().await {
                Ok(()) => return,
                Err(e) => {
                    if attempt == MAX_ACCEPT_RETRIES {
                        warn!(session = %self.session_id, error = %e, "route activation after accept failed");
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
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
    ///
    /// **Dual-source announcement**: the same file is also played on the
    /// opposite leg so both parties hear it (e.g. "call may be recorded").
    /// Playback completion is delivered on `side`'s handle; the caller is
    /// expected to restore the route (e.g. [`Self::resume`]) once done.
    pub async fn play_file(
        &mut self,
        side: LegSide,
        path: impl Into<String>,
        loop_playback: bool,
    ) -> Result<PlaybackHandle> {
        let path = path.into();
        let handle = self
            .play(
                side,
                Box::new(crate::audio_source::FileAudioSource::new(
                    path.clone(),
                    loop_playback,
                )
                .await?),
                loop_playback,
            )
            .await?;
        // Mirror onto the opposite leg so both parties hear the announcement.
        let other = side.opposite();
        if let Some(leg) = self.leg(other) {
            leg.set_egress_source(EgressSource::Media {
                audio: Box::new(
                    crate::audio_source::FileAudioSource::new(path, loop_playback).await?,
                ),
                loop_playback,
                on_end: None,
            })
            .await?;
        }
        Ok(handle)
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
    /// fast-path or transcode). Clears any active-play markers for both legs.
    pub async fn resume(&mut self, _side: LegSide) -> Result<()> {
        self.active_play.lock().clear();
        self.bridge().await
    }

    /// Mute a leg's egress (send silence on that side only). The opposite leg
    /// keeps its current route/egress untouched.
    pub async fn mute(&mut self, side: LegSide) -> Result<()> {
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        leg.set_egress_source(EgressSource::Silence).await
    }

    /// Send RFC 2833 telephone-event DTMF digits to a leg's remote peer.
    /// The digits ride the leg's own egress transport (SRTP-protected), on the
    /// negotiated telephone-event payload type, regardless of the active route
    /// (fast-path relay / transcode / hold all coexist with injected DTMF).
    pub async fn send_dtmf(&self, side: LegSide, digits: &str) -> Result<()> {
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        leg.send_dtmf(digits).await
    }

    /// Route a leg's egress to an external injection channel (MCU / conference
    /// mixer output). The returned sender pushes pre-encoded [`MediaSample`]s
    /// at the negotiated ptime cadence; when the channel is empty the pipeline
    /// emits silence so the outgoing stream never gaps.
    ///
    /// This replaces the legacy "add a sample track to the independent
    /// VoiceEnginePeer PC" conference output path: the mixer's mixed audio now
    /// rides the same MediaBridge leg that carries the call's media.
    pub fn inject(
        &self,
        side: LegSide,
    ) -> Result<tokio::sync::mpsc::Sender<rustrtc::media::MediaSample>> {
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        // Spawn the switch so we don't block the session loop on the egress
        // command channel.
        tokio::spawn(async move {
            let _ = leg.set_egress_source(EgressSource::Inject {
                rx: parking_lot::Mutex::new(rx),
            });
        });
        Ok(tx)
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
    use crate::negotiate::CodecInfo;

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

    #[tokio::test]
    async fn rtp_legs_negotiate_and_bridge_fastpath() {
        use rustrtc::SdpType;

        // Two RTP legs negotiate UAC/UAS-style SDP with each other (no DTLS),
        // then the bridge activates the same-codec fast-path relay.
        let mut mb = MediaBridge::new("s6", BridgeOpts::default());
        let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();

        let a_offer = a.create_offer(vec![]).await.expect("a offer");
        let b_answer = b
            .apply_sdp(&a_offer, SdpType::Offer)
            .await
            .expect("b answers a");
        a.apply_sdp(&b_answer, SdpType::Answer)
            .await
            .expect("a applies answer");

        assert!(a.negotiated().is_some(), "leg A should be negotiated");
        assert!(b.negotiated().is_some(), "leg B should be negotiated");

        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;

        assert!(mb.is_bridged(), "route should be active after both answer");

        // Same codec (PCMU) → fast-path relay on both legs.
        for side in [LegSide::A, LegSide::B] {
            let leg = mb.leg(side).expect("leg");
            assert!(
                leg.egress_is_relay(),
                "leg {side:?} should use fast-path relay"
            );
        }

        // Re-bridging the same codec pair is a no-op (idempotent).
        let _ = mb.bridge().await;
        assert!(mb.is_bridged());
        mb.close();
    }

    #[tokio::test]
    async fn webrtc_legs_negotiate_and_bridge_fastpath() {
        use rustrtc::SdpType;

        // Two WebRtc (DTLS-SRTP) legs negotiate UAC/UAS-style SDP with each
        // other and the bridge activates the same-codec (opus) fast-path.
        let cfg = LegConfig {
            transport: rustrtc::TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: audio_codec::CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("webrtc-test".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };
        let mut mb = MediaBridge::new("s7", BridgeOpts::default());
        let a = LegInner::new("a", &cfg).unwrap();
        let b = LegInner::new("b", &cfg).unwrap();

        let a_offer = a.create_offer(vec![]).await.expect("a offer");
        let b_answer = b
            .apply_sdp(&a_offer, SdpType::Offer)
            .await
            .expect("b answers a (webrtc)");
        a.apply_sdp(&b_answer, SdpType::Answer)
            .await
            .expect("a applies webrtc answer");

        assert!(a.negotiated().is_some());
        assert!(b.negotiated().is_some());

        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;

        assert!(mb.is_bridged(), "route should be active after both answer");
        mb.close();
    }

    #[tokio::test]
    async fn webrtc_rtp_cross_transport_bridge_transcodes() {
        use rustrtc::SdpType;

        // Cross-transport proxy scenario: leg A (WebRTC/opus) negotiates with
        // a WebRTC peer, leg B (RTP/PCMU) negotiates with an RTP peer, then the
        // bridge connects them. Different codecs → transcode (non-relay) route.
        let webrtc_cfg = LegConfig {
            transport: rustrtc::TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: audio_codec::CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("x-transport".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };

        let a = LegInner::new("a", &webrtc_cfg).unwrap();
        let a2 = LegInner::new("a2", &webrtc_cfg).unwrap();
        let a_offer = a.create_offer(vec![]).await.expect("a offer");
        let a2_answer = a2
            .apply_sdp(&a_offer, SdpType::Offer)
            .await
            .expect("a2 answers a");
        a.apply_sdp(&a2_answer, SdpType::Answer)
            .await
            .expect("a applies answer");

        let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
        let b2 = LegInner::new("b2", &LegConfig::rtp_pcmu()).unwrap();
        let b_offer = b.create_offer(vec![]).await.expect("b offer");
        let b2_answer = b2
            .apply_sdp(&b_offer, SdpType::Offer)
            .await
            .expect("b2 answers b");
        b.apply_sdp(&b2_answer, SdpType::Answer)
            .await
            .expect("b applies answer");

        assert!(a.negotiated().is_some());
        assert!(b.negotiated().is_some());

        let mut mb = MediaBridge::new("s8", BridgeOpts::default());
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;

        assert!(mb.is_bridged(), "cross-transport route should be active");
        // Different codecs (opus vs PCMU) → transcode path, not fast-path relay.
        assert!(
            !mb.leg(LegSide::A).unwrap().egress_is_relay(),
            "leg A should use transcode (not relay)"
        );
        assert!(
            !mb.leg(LegSide::B).unwrap().egress_is_relay(),
            "leg B should use transcode (not relay)"
        );
        mb.close();
    }

    /// Regression: bridging a WebRTC caller leg whose DTLS/SRTP transport is
    /// not yet ready (the remote only starts DTLS after it receives the 200 OK)
    /// with a same-codec RTP callee must NOT block the fast-path relay arming.
    /// Before the fix `Leg::set_egress_source(RewriteRelay)` synchronously
    /// waited up to 2s per leg for the WebRTC transport, which deadlocked call
    /// setup (the 200 OK is sent only after this returns). Now the arming is
    /// deferred to a background task and the call path returns immediately.
    #[tokio::test]
    async fn fastpath_relay_does_not_block_on_unready_webrtc_transport() {
        use rustrtc::SdpType;

        let webrtc_cfg = LegConfig {
            transport: rustrtc::TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: audio_codec::CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("unready-webrtc".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };
        let rtp_opus_cfg = LegConfig {
            transport: rustrtc::TransportMode::Rtp,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: audio_codec::CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("rtp-opus".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };

        // Leg A: WebRTC answerer whose remote (10.0.0.1) never connects, so its
        // SRTP transport stays unready even though the profile is negotiated.
        let webrtc_offer = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 5000 UDP/TLS/RTP/SAVPF 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n\
            a=setup:actpass\r\n\
            a=ice-ufrag:uv50\r\n\
            a=ice-pwd:ib8b\r\n\
            a=candidate:1 1 udp 2130706431 10.0.0.1 5000 typ host\r\n";
        let a = LegInner::new("a", &webrtc_cfg).unwrap();
        a.apply_sdp(webrtc_offer, SdpType::Offer)
            .await
            .expect("a answers webrtc offer");
        assert!(a.negotiated().is_some(), "leg A profile should be negotiated");

        // Leg B: RTP/opus — negotiated, transport ready.
        let b2 = LegInner::new("b2", &rtp_opus_cfg).unwrap();
        let b_offer = b2.create_offer(vec![]).await.expect("b2 offer");
        let b = LegInner::new("b", &rtp_opus_cfg).unwrap();
        b.apply_sdp(&b_offer, SdpType::Offer)
            .await
            .expect("b answers rtp offer");
        assert!(b.negotiated().is_some(), "leg B profile should be negotiated");

        let mut mb = MediaBridge::new("s-no-deadlock", BridgeOpts::default());
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;

        // Same codec (opus) on both legs → fast-path branch would hit
        // wait_for_rtp_transport_ready. It must NOT block now.
        let start = std::time::Instant::now();
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "accept must not block on the unready WebRTC transport (took {elapsed:?})"
        );
        assert!(mb.is_bridged(), "route should be active after both answer");
        mb.close();
    }

    /// Minimal sine-wave AudioSource for exercising leg→PCM data flow.
    struct SineSource {
        pcm: Vec<i16>,
        pos: usize,
        sample_rate: u32,
        channels: u16,
    }

    impl SineSource {
        fn new(freq: f64, sample_rate: u32, duration_ms: u64) -> Self {
            let n = (sample_rate as u64 * duration_ms / 1000) as usize;
            let mut pcm = Vec::with_capacity(n);
            for i in 0..n {
                let t = i as f64 / sample_rate as f64;
                pcm.push(((freq * 2.0 * std::f64::consts::PI * t).sin() * 8000.0) as i16);
            }
            Self {
                pcm,
                pos: 0,
                sample_rate,
                channels: 1,
            }
        }
    }

    impl crate::audio_source::AudioSource for SineSource {
        fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
            let n = buffer.len().min(self.pcm.len() - self.pos);
            buffer[..n].copy_from_slice(&self.pcm[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
        fn has_data(&self) -> bool {
            self.pos < self.pcm.len()
        }
        fn reset(&mut self) -> Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    /// P2.4 data-source migration: `leg_pcm_stream` decodes a leg's ingress
    /// RTP into PCM frames. Two RTP legs negotiate + bridge; leg A plays a
    /// sine tone; the stream on leg B must emit non-silence PCM.
    #[tokio::test]
    async fn leg_pcm_stream_decodes_bridged_audio() {
        use rustrtc::SdpType;

        let mut mb = MediaBridge::new("pcm-stream", BridgeOpts::default());
        let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();

        let a_offer = a.create_offer(vec![]).await.expect("a offer");
        let b_answer = b
            .apply_sdp(&a_offer, SdpType::Offer)
            .await
            .expect("b answers a");
        a.apply_sdp(&b_answer, SdpType::Answer)
            .await
            .expect("a applies answer");

        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;
        assert!(mb.is_bridged(), "route should be active after both answer");

        // Subscribe to leg B's PCM before playing, so we catch frames early.
        let mut stream = mb
            .leg_pcm_stream(LegSide::B)
            .expect("leg B PCM stream");

        // Play a 440 Hz tone on leg A (egress → RTP → B ingress).
        let handle = mb
            .play(LegSide::A, Box::new(SineSource::new(440.0, 8000, 1000)), false)
            .await
            .expect("play tone on A");
        let _ = handle;

        // Drain leg B PCM until we see a non-silence frame.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_audio = false;
        while tokio::time::Instant::now() < deadline {
            if let Some(frame) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                stream.recv(),
            )
            .await
            .ok()
            .flatten()
            {
                assert_eq!(frame.sample_rate, 8000, "PCMU leg decodes at 8 kHz");
                if !frame.silence && frame.samples.iter().any(|&s| s != 0) {
                    saw_audio = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw_audio, "leg B PCM stream must decode the tone played on A");
        mb.close();
    }
}
