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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use audio_codec::create_decoder;
use rustrtc::{MediaKind, RtpRewriteBridgeOptions, RtpRewriteRule, media::MediaStreamTrack};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::egress::EgressSource;
use crate::ingress_tap::{DtmfEvent, MediaRecorder, PacketDirection};
use crate::leg::Leg;
use crate::negotiate::{NegotiatedLegProfile, NegotiatedVideoCodec};

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
    /// Which leg(s) the recorder is attached to. `None` = both legs (legacy);
    /// `Some(side)` = only that leg. Recording captures the first leg's
    /// send+receive, so callers set this to `Some(LegSide::A)`.
    recorder_side: Option<LegSide>,
    dtmf_bus: broadcast::Sender<(LegSide, DtmfEvent)>,
    /// Root cancel token for all spawned sub-tasks (DTMF forwarders).
    root_cancel: CancellationToken,
    /// Per-leg cancel tokens for the `wire_leg` monitoring tasks. Cancelled
    /// when the leg is replaced (`replace_leg`) or the bridge is closed, so
    /// old monitoring tasks never leak across transfers / REFERs.
    leg_wire_cancels: HashMap<LegSide, CancellationToken>,
    /// Cancellation token for the current batch of RTCP-relay forwarder
    /// tasks (`wire_rtcp_sender_forward`). Regenerated on every `bridge()`
    /// fast-path activation; the previous batch is cancelled so those tasks
    /// exit promptly instead of pinning replaced legs (and their
    /// PeerConnections) alive for the rest of the session.
    rtcp_cancel: Option<CancellationToken>,
    /// Live count of RTCP-relay forwarder tasks (observability / leak
    /// regression tests). Incremented on spawn, decremented on task exit.
    rtcp_forwarder_count: Arc<AtomicUsize>,
    /// Legs currently playing a Media source. `play` inserts; the egress
    /// `on_end` callback removes.
    active_play: Arc<parking_lot::Mutex<HashSet<LegSide>>>,
    /// Codecs of the last successful bridge activation. Used to make
    /// `bridge()` idempotent: re-bridging the same codec pair on an already
    /// active route is a no-op (avoids rebuilding decoders/relay). The third
    /// element captures the relayed video codec identity (name + PT per leg),
    /// so a mid-call video codec change re-arms the relay rules.
    last_bridged:
        Option<(audio_codec::CodecType, audio_codec::CodecType, Option<(String, u8, String, u8)>)>,
}

impl MediaBridge {
    pub fn new(session_id: impl Into<String>, _opts: BridgeOpts) -> Self {
        let (dtmf_bus, _) = broadcast::channel(8);
        Self {
            session_id: session_id.into(),
            leg_a: None,
            leg_b: None,
            route_active: false,
            recorder: None,
            recorder_side: None,
            dtmf_bus,
            root_cancel: CancellationToken::new(),
            leg_wire_cancels: HashMap::new(),
            rtcp_cancel: None,
            rtcp_forwarder_count: Arc::new(AtomicUsize::new(0)),
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
    pub fn leg_pcm_stream(&self, side: LegSide) -> Result<crate::app_ingress::LegPcmStream> {
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

    /// Number of live RTCP-relay forwarder tasks (observability / leak
    /// regression tests). Returns to the per-activation count (0 after
    /// `unbridge`, 2 for an audio-only bridge) once cancelled generations
    /// have exited.
    pub fn active_rtcp_forwarders(&self) -> usize {
        self.rtcp_forwarder_count.load(Ordering::Relaxed)
    }

    /// Wire a leg's DTMF into the bridge bus, attach the default recorder, and
    /// monitor RTP inactivity timeout — all in ONE per-leg task (no extra
    /// spawns). The timeout check runs on a fixed 100ms interval and uses the
    /// leg's ingress packet counter + `armed_at` timestamp: when armed and no
    /// new packets arrive within the duration, the oneshot receiver is fired.
    fn wire_leg(&mut self, side: LegSide, leg: &Leg) {
        // Cancel any prior monitor task for this side first (e.g. after a
        // transfer / REFER replaced the leg) so old tasks don't leak and keep
        // the old PeerConnection / IngressTap / RTP timeout state alive.
        if let Some(old) = self.leg_wire_cancels.insert(side, self.root_cancel.child_token()) {
            old.cancel();
        }
        let cancel = self.leg_wire_cancels.get(&side).cloned().expect("just inserted");
        let mut rx = leg.subscribe_dtmf();
        let tap = leg.ingress_tap().clone();
        let timeout = leg.rtp_timeout_state();
        let leg_ref = leg.clone();
        let bus = self.dtmf_bus.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_count = tap.ingress_packet_count();
            // Only poll the 100ms interval while a timeout is actually armed.
            // Most legs never arm one (plain P2P), so polling unconditionally
            // would wake 1600 tasks at 10 Hz for nothing. The arm/resume paths
            // notify `timeout.armed`, waking this loop to resume polling.
            let mut monitoring = false;
            loop {
                let armed = timeout.armed_at.lock().is_some();
                if armed && !monitoring {
                    monitoring = true;
                    interval.reset();
                    last_count = tap.ingress_packet_count();
                } else if !armed {
                    monitoring = false;
                }
                let monitor: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                    if monitoring {
                        Box::pin(async {
                            interval.tick().await;
                        })
                    } else {
                        Box::pin(timeout.armed.notified())
                    };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Ok(ev) if ev.direction == PacketDirection::Ingress => {
                            let _ = bus.send((side, ev));
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    _ = monitor => {
                        if !armed || !monitoring {
                            continue;
                        }
                        if !timeout.active.load(Ordering::Relaxed)
                            || timeout.app_paused.load(Ordering::Relaxed)
                        {
                            // Idle / paused / app-suppressed — keep the baseline
                            // in sync.
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
            // Attach only when this leg matches the configured recorder side
            // (or when no side filter is set → both legs, legacy behaviour).
            // Recording is first-leg-only, so a B leg added later (e.g. after
            // a transfer) must NOT pick up the recorder.
            if self.recorder_side.is_none() || self.recorder_side == Some(side) {
                leg.set_recorder(Some(rec.clone()));
            }
        }
    }

    /// Place (or replace) a leg. If a route is already active, the codec is
    /// re-evaluated and the mode switches fast-path ↔ transcode as needed.
    /// Use this for call transfer / REFER.
    pub async fn replace_leg(&mut self, side: LegSide, new_leg: Leg) {
        self.wire_leg(side, &new_leg);
        let old = match side {
            LegSide::A => self.leg_a.replace(new_leg),
            LegSide::B => self.leg_b.replace(new_leg),
        };
        // The replaced leg is no longer part of this bridge (transfer /
        // REFER). Stop its PeerConnection NOW so ICE/DTLS/SRTP resources are
        // released even while other Arc holders (e.g. a still-exiting RTCP
        // relay task, cancelled via `rtcp_cancel`) linger on the wrapper.
        if let Some(old) = old {
            old.stop();
        }
        // The replaced leg's RTCP forwarders reference the old transport(s).
        // Cancel that generation now: if the new leg is not yet negotiated,
        // `bridge()` below returns early without regenerating the token, so
        // without this the old tasks would keep pinning the replaced leg.
        if let Some(old) = self.rtcp_cancel.take() {
            old.cancel();
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

    /// Attach the recorder to BOTH legs (legacy behaviour). Prefer
    /// [`Self::set_recorder_for`] for first-leg-only recording.
    pub fn set_recorder(&mut self, recorder: Arc<dyn MediaRecorder>) {
        self.recorder_side = None;
        if let Some(la) = self.leg_a.as_ref() {
            la.set_recorder(Some(recorder.clone()));
        }
        if let Some(lb) = self.leg_b.as_ref() {
            lb.set_recorder(Some(recorder.clone()));
        }
        self.recorder = Some(recorder);
    }

    /// Attach the recorder to a SINGLE leg (`side`). The stored side filter is
    /// honoured by `wire_leg`, so a leg created later (e.g. the B leg after a
    /// transfer) only receives the recorder when it matches `side`.
    ///
    /// Recording captures the first leg's send+receive, so callers pass
    /// [`LegSide::A`]: ingress = caller voice, egress = what the caller hears
    /// (IVR prompt / callee audio / hold music), all in the caller's codec.
    pub fn set_recorder_for(&mut self, side: LegSide, recorder: Arc<dyn MediaRecorder>) {
        self.recorder_side = Some(side);
        self.recorder = Some(recorder.clone());
        if let Some(leg) = self.leg(side) {
            leg.set_recorder(Some(recorder));
        }
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
        self.recorder_side = None;
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

        // Video relay match: a codec common to both legs (by case-insensitive
        // name). Relayed at the transport level (no transcoding); `None` when
        // the legs share no video codec → audio-only.
        let video_match =
            crate::negotiate::MediaNegotiator::find_common_video_codec(&pa.video, &pb.video);

        // Idempotent re-bridge: same codec pair on an already-active route is
        // a no-op (avoid rebuilding decoders / re-arming the relay).
        let bridged_key = (
            ca.codec,
            cb.codec,
            video_match
                .as_ref()
                .map(|(va, vb)| (va.name.clone(), va.payload_type, vb.name.clone(), vb.payload_type)),
        );
        if self.route_active && self.last_bridged.as_ref() == Some(&bridged_key) {
            return Ok(());
        }
        self.last_bridged = Some(bridged_key);

        if ca.codec == cb.codec {
            // ── fast-path: transport-level zero-copy relay ──
            debug!(session = %self.session_id, codec = ?ca.codec, "MBRIDGE fast-path relay"); // Rewrite the forwarded packet's header to the destination leg's
            // negotiated SSRC / PT, and strip WebRTC extension headers when the
            // destination is plain RTP.
            //
            // SSRC selection for relayed audio:
            // Both directions use a distinct random SSRC to avoid timeline
            // pollution on the playback SSRC. The rewrite bridge stamps the
            // destination's SDES-MID extension header on forwarded packets
            // (when the destination is WebRTC), so the browser attributes
            // them to the correct audio track regardless of SSRC.
            let a_transport = la.pc().config().transport_mode.clone();
            let b_transport = lb.pc().config().transport_mode.clone();
            let a_playback_ssrc = crate::leg::sender_ssrc_for_kind(la.pc(), MediaKind::Audio);
            let b_playback_ssrc = crate::leg::sender_ssrc_for_kind(lb.pc(), MediaKind::Audio);
            let a_relay_ssrc = distinct_relay_ssrc(a_playback_ssrc);
            let b_relay_ssrc = distinct_relay_ssrc(b_playback_ssrc);
            let a_video_ssrc = crate::leg::sender_ssrc_for_kind(la.pc(), rustrtc::MediaKind::Video);
            let b_video_ssrc = crate::leg::sender_ssrc_for_kind(lb.pc(), rustrtc::MediaKind::Video);
            // SDES-MID (ext id, value) per destination m-line: audio rules stamp
            // the audio mid, video rules the video mid — so a WebRTC receiver
            // demuxes audio vs video to the correct tracks (a direction-level
            // single mid would stamp audio's mid onto video packets and break
            // one-way video).
            let a_audio_mid = sdes_mid_for_kind(&la, MediaKind::Audio);
            let b_audio_mid = sdes_mid_for_kind(&lb, MediaKind::Audio);
            let a_video_mid = sdes_mid_for_kind(&la, rustrtc::MediaKind::Video);
            let b_video_mid = sdes_mid_for_kind(&lb, rustrtc::MediaKind::Video);

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

            // ── A→B rules: audio catch-all + DTMF + video ──
            let b_audio_mid_fields = b_audio_mid
                .as_ref()
                .map(|m| (m.0, Some(m.1.to_string())))
                .map(|(id, mid)| (Some(id), mid))
                .unwrap_or((None, None));
            let mut rules_a_to_b = vec![RtpRewriteRule {
                match_payload_type: None,
                fixed_out_ssrc: Some(b_relay_ssrc),
                ssrc_offset: 0,
                out_payload_type: (ca.payload_type != cb.payload_type).then_some(cb.payload_type),
                sdes_mid_extension_id: b_audio_mid_fields.0,
                sdes_mid: b_audio_mid_fields.1.clone(),
            }];
            if let Some((a_pt, b_pt)) = dtmf_a_to_b {
                rules_a_to_b.push(RtpRewriteRule {
                    match_payload_type: Some(a_pt),
                    fixed_out_ssrc: Some(b_relay_ssrc),
                    ssrc_offset: 0,
                    out_payload_type: Some(b_pt),
                    sdes_mid_extension_id: b_audio_mid_fields.0,
                    sdes_mid: b_audio_mid_fields.1.clone(),
                });
            }
            let (video_a_to_b, video_b_to_a) = video_relay_rules(
                &pa.video,
                &pb.video,
                a_video_ssrc,
                b_video_ssrc,
                pa.audio.as_ref().map(|a| a.payload_type),
                pa.dtmf_pts(),
                pb.audio.as_ref().map(|a| a.payload_type),
                pb.dtmf_pts(),
                a_video_mid,
                b_video_mid,
            );
            rules_a_to_b.extend(video_a_to_b);

            // ── B→A rules (mirror) ──
            let a_audio_mid_fields = a_audio_mid
                .as_ref()
                .map(|m| (m.0, Some(m.1.to_string())))
                .map(|(id, mid)| (Some(id), mid))
                .unwrap_or((None, None));
            let mut rules_b_to_a = vec![RtpRewriteRule {
                match_payload_type: None,
                fixed_out_ssrc: Some(a_relay_ssrc),
                ssrc_offset: 0,
                out_payload_type: (ca.payload_type != cb.payload_type).then_some(ca.payload_type),
                sdes_mid_extension_id: a_audio_mid_fields.0,
                sdes_mid: a_audio_mid_fields.1.clone(),
            }];
            if let Some((a_pt, b_pt)) = dtmf_b_to_a {
                rules_b_to_a.push(RtpRewriteRule {
                    match_payload_type: Some(b_pt),
                    fixed_out_ssrc: Some(a_relay_ssrc),
                    ssrc_offset: 0,
                    out_payload_type: Some(a_pt),
                    sdes_mid_extension_id: a_audio_mid_fields.0,
                    sdes_mid: a_audio_mid_fields.1.clone(),
                });
            }
            rules_b_to_a.extend(video_b_to_a);

            let options_a_to_b = RtpRewriteBridgeOptions {
                strip_extensions: b_transport == rustrtc::TransportMode::Rtp,
                ..Default::default()
            };
            let options_b_to_a = RtpRewriteBridgeOptions {
                strip_extensions: a_transport == rustrtc::TransportMode::Rtp,
                ..Default::default()
            };

            info!(
                session = %self.session_id,
                codec = ?ca.codec,
                a_playback_ssrc, b_playback_ssrc, a_relay_ssrc, b_relay_ssrc,
                video = ?video_match.as_ref().map(|(v, _)| v.name.as_str()),
                strip_a_to_b = options_a_to_b.strip_extensions,
                strip_b_to_a = options_b_to_a.strip_extensions,
                "fast-path relay activated"
            );

            la.set_egress_source(EgressSource::RewriteRelay {
                peer_pc: lb.pc().clone(),
                options: options_a_to_b,
                rules: rules_a_to_b,
            })
            .await?;
            lb.set_egress_source(EgressSource::RewriteRelay {
                peer_pc: la.pc().clone(),
                options: options_b_to_a,
                rules: rules_b_to_a,
            })
            .await?;
            // WebRTC receivers depend on RTCP PLI/NACK to recover lost video
            // keyframes; the RTP relay forwards only RTP, so relay the feedback
            // across the legs (rewriting media_ssrc to the peer's real sender
            // SSRC). Without this, a missed initial keyframe is unrecoverable →
            // one-way black video.
            // Regenerate the RTCP-relay cancellation generation: the previous
            // batch of forwarder tasks (from an earlier bridge activation or a
            // replaced leg) exits immediately instead of pinning old legs alive.
            let rtcp_cancel = self.root_cancel.child_token();
            if let Some(old) = self.rtcp_cancel.replace(rtcp_cancel.clone()) {
                old.cancel();
            }
            // The ingress SSRC→PT map that the relay reads is only needed when a
            // WebRTC leg is involved; for a plain RTP↔RTP bridge (no WebRTC on
            // either side) the relay has nothing to rewrite, so disable tracking
            // on both legs to skip the per-packet DashMap write entirely.
            // (The relay itself stays wired — RTCP forwarders are no-ops for
            // RTP-only legs and upstream's cancellation lifecycle depends on it.)
            let needs_ssrc_pt_tracking = a_transport == rustrtc::TransportMode::WebRtc
                || b_transport == rustrtc::TransportMode::WebRtc;
            la.ingress_tap()
                .set_track_ingress_ssrc_pts(needs_ssrc_pt_tracking);
            lb.ingress_tap()
                .set_track_ingress_ssrc_pts(needs_ssrc_pt_tracking);
            wire_rtcp_relay(
                &la,
                &lb,
                &pa,
                &pb,
                rtcp_cancel,
                self.rtcp_forwarder_count.clone(),
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
                source_audio_payload_type: cb.payload_type,
            })
            .await?;
            lb.set_egress_source(EgressSource::TranscodePeer {
                peer: a_recv,
                decoder: a_decoder,
                src_sample_rate: a_src_rate,
                source_audio_payload_type: ca.payload_type,
            })
            .await?;
            info!(
                session = %self.session_id,
                a_codec = ?ca.codec,
                b_codec = ?cb.codec,
                "transcoding activated"
            );
            // No RTCP PLI/NACK relay in the transcode path (packets are decoded
            // and re-encoded), so the ingress SSRC→PT map is never read here —
            // disable tracking to skip the per-packet DashMap write.
            la.ingress_tap().set_track_ingress_ssrc_pts(false);
            lb.ingress_tap().set_track_ingress_ssrc_pts(false);
        }

        self.route_active = true;
        Ok(())
    }

    /// Break the route: both legs' egress → [`EgressSource::Silence`] and any
    /// rewrite bridge is torn down (handled inside `Leg::set_egress_source`).
    pub async fn unbridge(&mut self) -> Result<()> {
        self.route_active = false;
        self.last_bridged = None;
        if let Some(old) = self.rtcp_cancel.take() {
            old.cancel();
        }
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
        self.play_side_only(side, audio, loop_playback).await
    }

    /// Play a media source on a leg **without** breaking the opposite leg's
    /// egress.  Unlike [`Self::play`], this does NOT call `unbridge()` first,
    /// so the opposite leg keeps whatever it was playing (e.g. looping hold
    /// music during an `ivr.exec` flow).
    ///
    /// Should only be used when the route is already inactive (e.g. after a
    /// `hold()`).  If the route is active, use [`Self::play`] instead.
    pub async fn play_side_only(
        &mut self,
        side: LegSide,
        audio: Box<dyn crate::audio_source::AudioSource>,
        loop_playback: bool,
    ) -> Result<PlaybackHandle> {
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        leg.pause_rtp_timeout();
        let leg_for_end = leg.clone();
        let (handle, done_tx) = PlaybackHandle::new();
        self.active_play.lock().insert(side);
        let active_registry = self.active_play.clone();
        let done_tx = Arc::new(parking_lot::Mutex::new(Some(done_tx)));
        let on_end = Arc::new(move |interrupted: bool| {
            active_registry.lock().remove(&side);
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

    /// Play a file on a leg **without** breaking the opposite leg's egress.
    /// Convenience wrapper around [`Self::play_side_only`] for file sources.
    /// Unlike [`Self::play_file`], does NOT mirror onto the opposite leg.
    pub async fn play_file_side_only(
        &mut self,
        side: LegSide,
        path: impl Into<String>,
        loop_playback: bool,
    ) -> Result<PlaybackHandle> {
        let path = path.into();
        self.play_side_only(
            side,
            Box::new(
                crate::audio_source::FileAudioSource::new(path, loop_playback).await?,
            ),
            loop_playback,
        )
        .await
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
                Box::new(
                    crate::audio_source::FileAudioSource::new(path.clone(), loop_playback).await?,
                ),
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
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
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
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        match music {
            Some(audio) => {
                leg.set_egress_source(EgressSource::Media {
                    audio,
                    loop_playback: true,
                    on_end: None,
                })
                .await?
            }
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
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        leg.set_egress_source(EgressSource::Silence).await
    }

    /// Send RFC 2833 telephone-event DTMF digits to a leg's remote peer.
    /// The digits ride the leg's own egress transport (SRTP-protected), on the
    /// negotiated telephone-event payload type, regardless of the active route
    /// (fast-path relay / transcode / hold all coexist with injected DTMF).
    pub async fn send_dtmf(&self, side: LegSide, digits: &str) -> Result<()> {
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
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
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        // Spawn the switch so we don't block the session loop on the egress
        // command channel.
        tokio::spawn(async move {
            if let Err(e) = leg
                .set_egress_source(EgressSource::Inject {
                    rx: parking_lot::Mutex::new(rx),
                })
                .await
            {
                warn!(error = ?e, side = ?side, "inject: failed to switch leg egress to Inject");
            }
        });
        Ok(tx)
    }

    /// Set up a raw-PCM channel audio source on the given leg. The returned
    /// sender feeds the egress pipeline via [`ChannelAudioSource`]; on empty
    /// ticks the egress emits comfort-noise (CNG) instead of dead silence,
    /// courtesy of `loop_playback=true`.
    ///
    /// The source does NOT pre-encode — the leg's egress encoder converts
    /// PCM→codec at its own 20 ms cadence ("filetrack mode").
    pub async fn bridge_play_pcm(
        &self,
        side: LegSide,
        sample_rate: u32,
    ) -> Result<tokio::sync::mpsc::Sender<Vec<i16>>> {
        let leg = self.leg(side).ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let source = Box::new(crate::audio_source::ChannelAudioSource::new(rx, sample_rate));
        leg.play(source, true, None).await?;
        Ok(tx)
    }

    // ── Timeout / lifecycle ──────────────────────────────────────────────

    /// Arm an RTP inactivity timeout for a leg. Returns a `oneshot::Receiver`
    /// that fires (`Ok(())`) when no ingress packet arrives within `timeout`.
    /// Monitored by the per-leg DTMF task (no dedicated spawn).
    pub fn arm_rtp_timeout(
        &self,
        side: LegSide,
        timeout: Duration,
    ) -> Option<oneshot::Receiver<()>> {
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

    /// Set the app-level suppression flag for a leg. When `true` the monitor
    /// never fires regardless of arm/pause state — used while an app
    /// (IVR/voicemail/queue) drives the session or during a blind transfer.
    pub fn set_app_paused(&self, side: LegSide, paused: bool) {
        if let Some(leg) = self.leg(side) {
            leg.set_app_paused(paused);
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
        self.route_active = false;
        self.teardown();
    }

    /// Cancel tasks and stop legs. Legs are stopped synchronously (the rustrtc
    /// close path has no tokio::spawn, so this never panics during runtime
    /// teardown).
    fn teardown(&mut self) {
        self.root_cancel.cancel();
        if let Some(old) = self.rtcp_cancel.take() {
            old.cancel();
        }
        for (_, cancel) in self.leg_wire_cancels.drain() {
            cancel.cancel();
        }
        if let Some(la) = self.leg_a.take() {
            la.stop();
        }
        if let Some(lb) = self.leg_b.take() {
            lb.stop();
        }
    }
}

impl Drop for MediaBridge {
    fn drop(&mut self) {
        self.teardown();
    }
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

fn distinct_relay_ssrc(playback_ssrc: u32) -> u32 {
    loop {
        let ssrc = rand::random::<u32>();
        if ssrc != 0 && ssrc != playback_ssrc {
            return ssrc;
        }
    }
}

/// Read the SDES-MID (extension id, mid value) from a leg's sender for the
/// given media kind — the tuple the rewrite bridge needs to stamp the MID
/// header extension on forwarded packets so a WebRTC receiver can attribute
/// them to the negotiated track regardless of their (relay) SSRC.
fn sdes_mid_for_kind(leg: &Leg, kind: rustrtc::MediaKind) -> Option<(u8, std::sync::Arc<str>)> {
    leg.pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == kind)
        .and_then(|t| t.sender())
        .and_then(|s| s.sdes_mid())
}

/// Build the video payload-type rewrite rules for the fast-path relay.
///
/// The relay must match **every** video payload type a leg may actually send,
/// not just the first common codec. Each WebRTC peer picks its own send codec
/// (offerer vs answerer, hardware vs software encoder), so a leg may send any
/// negotiated profile — VP8 **or** one of the H264 variants. An unmatched
/// video PT falls through to the audio catch-all rule and gets stamped with
/// the audio SSRC, which the peer drops: a persistent one-way video failure
/// (the peer sending on the covered PT still works, the other side is black).
///
/// For each video codec on leg A we add a rule matching that PT and rewriting
/// it to the peer leg's PT for the same (name, fmtp) codec, stamped with the
/// destination leg's video sender SSRC; the mirror covers B→A. PTs that
/// collide with a leg's own audio / DTMF payload types are skipped so the
/// video rules never hijack the audio stream.
#[allow(clippy::too_many_arguments)]
fn video_relay_rules(
    a: &[NegotiatedVideoCodec],
    b: &[NegotiatedVideoCodec],
    a_video_ssrc: u32,
    b_video_ssrc: u32,
    a_audio_pt: Option<u8>,
    a_dtmf_pts: std::collections::HashSet<u8>,
    b_audio_pt: Option<u8>,
    b_dtmf_pts: std::collections::HashSet<u8>,
    a_video_mid: Option<(u8, std::sync::Arc<str>)>,
    b_video_mid: Option<(u8, std::sync::Arc<str>)>,
) -> (Vec<RtpRewriteRule>, Vec<RtpRewriteRule>) {
    fn match_peer<'x>(
        codec: &NegotiatedVideoCodec,
        peer: &'x [NegotiatedVideoCodec],
    ) -> Option<&'x NegotiatedVideoCodec> {
        // Prefer an exact (name, fmtp) match so H264 profiles map 1:1 (their
        // PTs are preserved across legs by the codec builder); fall back to a
        // name-only match when the fmtp differs between legs.
        peer.iter()
            .find(|c| c.name.eq_ignore_ascii_case(&codec.name) && c.fmtp == codec.fmtp)
            .or_else(|| peer.iter().find(|c| c.name.eq_ignore_ascii_case(&codec.name)))
    }

    fn is_audio_pt(pt: u8, audio_pt: Option<u8>, dtmf_pts: &std::collections::HashSet<u8>) -> bool {
        audio_pt == Some(pt) || dtmf_pts.contains(&pt)
    }

    let mid_fields = |m: &Option<(u8, std::sync::Arc<str>)>| {
        m.as_ref()
            .map(|m| (m.0, m.1.to_string()))
            .map(|(id, mid)| (Some(id), Some(mid)))
            .unwrap_or((None, None))
    };
    let (b_mid_id, b_mid) = mid_fields(&b_video_mid);
    let (a_mid_id, a_mid) = mid_fields(&a_video_mid);

    let mut a_to_b = Vec::new();
    for va in a {
        if is_audio_pt(va.payload_type, a_audio_pt, &a_dtmf_pts) {
            continue;
        }
        if let Some(vb) = match_peer(va, b) {
            a_to_b.push(RtpRewriteRule {
                match_payload_type: Some(va.payload_type),
                fixed_out_ssrc: Some(b_video_ssrc),
                ssrc_offset: 0,
                out_payload_type: Some(vb.payload_type),
                sdes_mid_extension_id: b_mid_id,
                sdes_mid: b_mid.clone(),
            });
        }
    }

    let mut b_to_a = Vec::new();
    for vb in b {
        if is_audio_pt(vb.payload_type, b_audio_pt, &b_dtmf_pts) {
            continue;
        }
        if let Some(va) = match_peer(vb, a) {
            b_to_a.push(RtpRewriteRule {
                match_payload_type: Some(vb.payload_type),
                fixed_out_ssrc: Some(a_video_ssrc),
                ssrc_offset: 0,
                out_payload_type: Some(va.payload_type),
                sdes_mid_extension_id: a_mid_id,
                sdes_mid: a_mid.clone(),
            });
        }
    }

    (a_to_b, b_to_a)
}

/// The RTP transport used to send RTCP to a leg's remote peer (the PC's
/// muxed media transport, from the video sender — all media shares it).
fn leg_send_transport(leg: &Leg, kind: MediaKind) -> Option<Arc<rustrtc::transports::rtp::RtpTransport>> {
    leg.pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == kind)
        .and_then(|t| t.sender())
        .and_then(|s| s.transport())
}

/// Wire RTCP feedback relay for the fast-path RTP relay.
///
/// The RTP rewrite bridge forwards only RTP; each leg's RTCP (PLI / NACK) is
/// consumed locally by rustrtc. A WebRTC receiver depends on PLI to recover a
/// lost initial keyframe and NACK to recover lost packets — without them, a
/// single missed keyframe is unrecoverable and the video stays black in one
/// direction. This subscribes to each leg's audio/video sender feedback and
/// forwards PLI/NACK to the peer leg's transport, rewriting `media_ssrc` from
/// the *relayed* SSRC back onto the peer browser's real sender SSRC (observed
/// on the peer leg's ingress tap) so the peer's encoder responds.
fn wire_rtcp_relay(
    a: &Leg,
    b: &Leg,
    pa: &NegotiatedLegProfile,
    pb: &NegotiatedLegProfile,
    cancel: CancellationToken,
    forwarder_count: Arc<AtomicUsize>,
) {
    let a_video_pts: Vec<u8> = pa.video.iter().map(|v| v.payload_type).collect();
    let b_video_pts: Vec<u8> = pb.video.iter().map(|v| v.payload_type).collect();
    let a_audio_pt = pa.audio.as_ref().map(|c| c.payload_type);
    let b_audio_pt = pb.audio.as_ref().map(|c| c.payload_type);

    // Bob's feedback (leg A senders) → forward to alice (leg B transport).
    wire_rtcp_sender_forward(
        a.clone(),
        b.clone(),
        MediaKind::Video,
        b_video_pts,
        cancel.clone(),
        forwarder_count.clone(),
    );
    let b_audio_pts: Vec<u8> = b_audio_pt.into_iter().collect();
    wire_rtcp_sender_forward(
        a.clone(),
        b.clone(),
        MediaKind::Audio,
        b_audio_pts,
        cancel.clone(),
        forwarder_count.clone(),
    );

    // Alice's feedback (leg B senders) → forward to bob (leg A transport).
    wire_rtcp_sender_forward(
        b.clone(),
        a.clone(),
        MediaKind::Video,
        a_video_pts,
        cancel.clone(),
        forwarder_count.clone(),
    );
    let a_audio_pts: Vec<u8> = a_audio_pt.into_iter().collect();
    wire_rtcp_sender_forward(b.clone(), a.clone(), MediaKind::Audio, a_audio_pts, cancel, forwarder_count);
}

/// Spawn one RTCP-forwarding task: feedback (PLI/NACK) targeting a sender of
/// `src_leg` is rewritten to the peer's real sender SSRC (looked up from
/// `dst_leg`'s ingress tap for `dst_pts`) and pushed to the peer via
/// `dst_leg`'s send transport.
///
/// The sender transports only exist after the DTLS/SRTP handshake completes
/// (post-answer), so the task waits for them before subscribing/forwarding.
fn wire_rtcp_sender_forward(
    src_leg: Leg,
    dst_leg: Leg,
    kind: MediaKind,
    dst_pts: Vec<u8>,
    cancel: CancellationToken,
    forwarder_count: Arc<AtomicUsize>,
) {
    use rustrtc::rtp::{GenericNack, PictureLossIndication, RtcpPacket};

    let sender = src_leg
        .pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == kind)
        .and_then(|t| t.sender());
    let Some(sender) = sender else { return };
    if dst_pts.is_empty() {
        return;
    }
    let mut rx = sender.subscribe_rtcp();
    let dst_tap = dst_leg.ingress_tap().clone();
    forwarder_count.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        // Decrement on EVERY exit path so the leak regression counter stays
        // accurate even when the task aborts early (cancelled / no transport).
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _guard = Guard(forwarder_count);
        // Wait for the destination leg's send transport (created on DTLS/SRTP
        // setup after the remote answers) — a few seconds max. Abort early if
        // the relay generation is cancelled (e.g. leg replaced / unbridge).
        let mut dst_tx = None;
        for _ in 0..50 {
            dst_tx = leg_send_transport(&dst_leg, kind)
                .or_else(|| leg_send_transport(&dst_leg, MediaKind::Audio));
            if dst_tx.is_some() {
                break;
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
        let Some(dst_tx) = dst_tx else {
            return;
        };

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                packet = rx.recv() => {
                    let Ok(packet) = packet else { break };
                    // The peer's real sender SSRC for this media type; skip
                    // until the peer has actually started sending it.
                    let Some(target) = dst_tap.ingress_ssrc_for_pts(&dst_pts) else {
                        continue;
                    };
                    let forwarded = match &packet {
                        RtcpPacket::PictureLossIndication(p) => {
                            RtcpPacket::PictureLossIndication(PictureLossIndication {
                                sender_ssrc: p.sender_ssrc,
                                media_ssrc: target,
                            })
                        }
                        RtcpPacket::GenericNack(n) => RtcpPacket::GenericNack(GenericNack {
                            sender_ssrc: n.sender_ssrc,
                            media_ssrc: target,
                            lost_packets: n.lost_packets.clone(),
                        }),
                        _ => continue,
                    };
                    if dst_tx.send_rtcp(&[forwarded]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

// Re-export the profile type for callers that inspect legs.
pub use crate::negotiate::NegotiatedLegProfile as LegProfile;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leg::{LegConfig, LegInner};
    use crate::negotiate::CodecInfo;
    use crate::test_utils::CountingRecorder;

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
            fn write_sample(
                &self,
                _: crate::ingress_tap::PacketDirection,
                _: &rustrtc::rtp::RtpPacket,
            ) {
            }
            fn write_dtmf(&self, _: DtmfEvent) {}
            fn set_paused(&self, _: bool) {}
            fn finalize(&self) {}
        }
        let mut mb = MediaBridge::new("s3", BridgeOpts::default());
        mb.replace_leg(
            LegSide::A,
            LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;
        mb.set_recorder(Arc::new(Noop));
        mb.replace_leg(
            LegSide::B,
            LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;
        let _ = mb.leg(LegSide::B).unwrap().stats();
        mb.close();
    }

    /// A recorder that counts samples per direction, used to verify which leg
    /// received the recorder.

    fn feed_packet(tap: &crate::ingress_tap::IngressTap, ingress: bool) {
        use rustrtc::peer_connection::RtpObserver;
        use rustrtc::rtp::{RtpHeader, RtpPacket};
        let pkt = RtpPacket::new(RtpHeader::new(0, 1, 160, 1234), vec![0xFFu8; 80]);
        let addr: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();
        if ingress {
            tap.on_ingress(&pkt, addr);
        } else {
            tap.on_egress(&pkt, addr);
        }
    }

    /// `set_recorder_for(A)` mounts the recorder on the A leg only. Feeding
    /// packets to A's tap (both directions) records them; feeding B's tap
    /// records nothing. This is the core fix for the recording-stutter bug
    /// where both legs were merged into the same channel.
    #[tokio::test]
    async fn set_recorder_for_attaches_only_to_a_leg() {
        let rec = Arc::new(CountingRecorder::new());
        let mut mb = MediaBridge::new("s-side-a", BridgeOpts::default());
        // A leg created AFTER set_recorder_for (mirrors real timing: the
        // recorder is configured at session construction, legs arrive later).
        mb.set_recorder_for(LegSide::A, rec.clone());
        mb.replace_leg(
            LegSide::A,
            LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;
        mb.replace_leg(
            LegSide::B,
            LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;

        let a_tap = mb.leg(LegSide::A).unwrap().ingress_tap().clone();
        let b_tap = mb.leg(LegSide::B).unwrap().ingress_tap().clone();
        // Feed A ingress + egress → both recorded.
        feed_packet(&a_tap, true);
        feed_packet(&a_tap, false);
        // Feed B ingress + egress → must NOT be recorded.
        feed_packet(&b_tap, true);
        feed_packet(&b_tap, false);

        assert_eq!(
            rec.ingress.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "only A-leg ingress must be recorded"
        );
        assert_eq!(
            rec.egress.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "only A-leg egress must be recorded (IVR / callee audio)"
        );
        mb.close();
    }

    /// After transfer replaces the B leg, the recorder must NOT migrate to the
    /// new B leg — recording stays anchored to A.
    #[tokio::test]
    async fn set_recorder_for_survives_b_leg_replacement() {
        let rec = Arc::new(CountingRecorder::new());
        let mut mb = MediaBridge::new("s-xfer", BridgeOpts::default());
        mb.replace_leg(
            LegSide::A,
            LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;
        mb.replace_leg(
            LegSide::B,
            LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;
        mb.set_recorder_for(LegSide::A, rec.clone());
        // Simulate transfer: replace B.
        mb.replace_leg(
            LegSide::B,
            LegInner::new("b2", &LegConfig::rtp_pcmu()).unwrap(),
        )
        .await;

        let b2_tap = mb.leg(LegSide::B).unwrap().ingress_tap().clone();
        feed_packet(&b2_tap, true);
        feed_packet(&b2_tap, false);
        assert_eq!(
            rec.ingress.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "replaced B leg must not be recorded"
        );
        assert_eq!(
            rec.egress.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "replaced B leg must not be recorded"
        );
        mb.close();
    }

    #[tokio::test]
    async fn dtmf_bus_forwards_ingress_and_ignores_egress() {
        use rustrtc::peer_connection::RtpObserver;
        use rustrtc::rtp::{RtpHeader, RtpPacket};

        let mut mb = MediaBridge::new("s4", BridgeOpts::default());
        let leg = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
        leg.ingress_tap().set_dtmf_payload_types(vec![101]);
        mb.replace_leg(LegSide::A, leg.clone()).await;
        let mut rx = mb.dtmf_bus();
        let addr: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();

        let egress = RtpPacket::new(
            RtpHeader::new(101, 1, 100, 1234),
            vec![1, 0x80, 0, 160],
        );
        leg.ingress_tap().on_egress(&egress, addr);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "egress DTMF must not be published to the session bus"
        );

        let ingress = RtpPacket::new(
            RtpHeader::new(101, 2, 200, 1234),
            vec![2, 0x80, 0, 160],
        );
        leg.ingress_tap().on_ingress(&ingress, addr);
        let (side, event) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("ingress DTMF bus timeout")
            .expect("ingress DTMF bus closed");
        assert_eq!(side, LegSide::A);
        assert_eq!(event.digit, '2');
        assert_eq!(event.direction, PacketDirection::Ingress);

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
            video_codecs: Vec::new(),
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

    /// Two WebRTC legs that both carry VIDEO (VP8 + H264) negotiate and bridge
    /// over the fast-path relay. This exercises the all-payload-type video
    /// rewrite rules AND the RTCP PLI/NACK relay wiring (`wire_rtcp_relay`)
    /// — regression guard for the one-way black-video bug. In the unit-test
    /// environment the DTLS transports never come up, so the RTCP-forwarding
    /// tasks must degrade gracefully (no panic, no spin) while the RTP relay
    /// rules are still installed and both legs report fast-path egress.
    #[tokio::test]
    async fn webrtc_video_legs_bridge_fastpath_with_rtcp_relay() {
        use rustrtc::SdpType;

        let cfg = LegConfig {
            transport: rustrtc::TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: audio_codec::CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            video_codecs: crate::negotiate::MediaNegotiator::default_video_codecs(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("webrtc-video".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };
        let mut mb = MediaBridge::new("s-video-rtcp", BridgeOpts::default());
        let a = LegInner::new("a", &cfg).unwrap();
        let b = LegInner::new("b", &cfg).unwrap();

        let a_offer = a.create_offer(vec![]).await.expect("a offer");
        let b_answer = b
            .apply_sdp(&a_offer, SdpType::Offer)
            .await
            .expect("b answers a (webrtc video)");
        a.apply_sdp(&b_answer, SdpType::Answer)
            .await
            .expect("a applies webrtc video answer");

        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;

        assert!(mb.is_bridged(), "route should be active after both answer");
        for side in [LegSide::A, LegSide::B] {
            let leg = mb.leg(side).expect("leg");
            assert!(leg.egress_is_relay(), "leg {side:?} should use fast-path relay");
        }

        // Give the RTCP-forwarding tasks a moment: with no DTLS transport in
        // the unit-test env they must time out quietly (never panic/spin).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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
            video_codecs: Vec::new(),
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
            video_codecs: Vec::new(),
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
            video_codecs: Vec::new(),
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
        assert!(
            a.negotiated().is_some(),
            "leg A profile should be negotiated"
        );

        // Leg B: RTP/opus — negotiated, transport ready.
        let b2 = LegInner::new("b2", &rtp_opus_cfg).unwrap();
        let b_offer = b2.create_offer(vec![]).await.expect("b2 offer");
        let b = LegInner::new("b", &rtp_opus_cfg).unwrap();
        b.apply_sdp(&b_offer, SdpType::Offer)
            .await
            .expect("b answers rtp offer");
        assert!(
            b.negotiated().is_some(),
            "leg B profile should be negotiated"
        );

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
        let mut stream = mb.leg_pcm_stream(LegSide::B).expect("leg B PCM stream");

        // Play a 440 Hz tone on leg A (egress → RTP → B ingress).
        let handle = mb
            .play(
                LegSide::A,
                Box::new(SineSource::new(440.0, 8000, 1000)),
                false,
            )
            .await
            .expect("play tone on A");
        let _ = handle;

        // Drain leg B PCM until we see a non-silence frame.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_audio = false;
        while tokio::time::Instant::now() < deadline {
            if let Some(frame) =
                tokio::time::timeout(std::time::Duration::from_millis(500), stream.recv())
                    .await
                    .ok()
                    .flatten()
            {
                assert_eq!(frame.frame.sample_rate, 8000, "PCMU leg decodes at 8 kHz");
                if !frame.silence && frame.frame.samples.iter().any(|&s| s != 0) {
                    saw_audio = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            saw_audio,
            "leg B PCM stream must decode the tone played on A"
        );
        mb.close();
    }

    fn vcap(name: &str, pt: u8, fmtp: Option<&str>) -> NegotiatedVideoCodec {
        NegotiatedVideoCodec {
            name: name.to_string(),
            payload_type: pt,
            clock_rate: 90000,
            fmtp: fmtp.map(str::to_string),
            rtx_payload_type: None,
        }
    }

    fn empty_dtmf() -> std::collections::HashSet<u8> {
        std::collections::HashSet::new()
    }

    /// The fast-path relay must install a rewrite rule for EVERY negotiated
    /// video payload type, not just the first common codec. A browser may send
    /// video on any negotiated profile (VP8 or any H264 variant); an unmatched
    /// PT falls to the audio catch-all, is stamped with the AUDIO SSRC, and is
    /// dropped by the peer — the one-way video failure.
    #[test]
    fn video_relay_rules_cover_all_negotiated_video_pts() {
        // Real browser negotiation (mirrors a Chrome offer → bridge answer):
        // VP8 first, then several H264 profiles at preserved PTs (96..=124).
        let a = vec![
            vcap("VP8", 96, None),
            vcap("H264", 102, Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f")),
            vcap("H264", 104, Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f")),
            vcap("H264", 108, Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f")),
            vcap("H264", 114, Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f")),
            vcap("H264", 116, Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f")),
            vcap("H264", 39, Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=4d001f")),
            vcap("H264", 118, Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64001f")),
        ];
        let b = a.clone();

        let (a_to_b, b_to_a) = video_relay_rules(
            &a,
            &b,
            0xA0A0A0A0, // a_video_ssrc
            0xB0B0B0B0, // b_video_ssrc
            Some(111),
            empty_dtmf(),
            Some(111),
            empty_dtmf(),
            None,
            None,
        );

        let a_to_b_by_pt: std::collections::HashMap<u8, RtpRewriteRule> = a_to_b
            .iter()
            .map(|r| (r.match_payload_type.unwrap(), r.clone()))
            .collect();
        let b_to_a_by_pt: std::collections::HashMap<u8, RtpRewriteRule> = b_to_a
            .iter()
            .map(|r| (r.match_payload_type.unwrap(), r.clone()))
            .collect();

        // Every video PT on leg A must have a relay rule, stamped with leg B's
        // video sender SSRC; mirror for B→A.
        for va in &a {
            let rule = a_to_b_by_pt.get(&va.payload_type).unwrap_or_else(|| {
                panic!("no A→B relay rule for video PT {}", va.payload_type)
            });
            assert_eq!(rule.fixed_out_ssrc, Some(0xB0B0B0B0), "A→B must use B's video SSRC");
            assert_eq!(rule.out_payload_type, Some(va.payload_type), "PT preserved across legs");
        }
        for vb in &b {
            let rule = b_to_a_by_pt.get(&vb.payload_type).unwrap_or_else(|| {
                panic!("no B→A relay rule for video PT {}", vb.payload_type)
            });
            assert_eq!(rule.fixed_out_ssrc, Some(0xA0A0A0A0), "B→A must use A's video SSRC");
            assert_eq!(rule.out_payload_type, Some(vb.payload_type), "PT preserved across legs");
        }
    }

    /// Video rules must not hijack a leg's own audio / DTMF payload types —
    /// otherwise DTMF events (or the audio stream) would be rewritten to the
    /// video SSRC and the peer would drop them.
    #[test]
    fn video_relay_rules_skip_audio_and_dtmf_pts() {
        let a = vec![vcap("VP8", 96, None), vcap("H264", 110, None)];
        let b = vec![vcap("VP8", 96, None), vcap("H264", 110, None)];
        let mut a_dtmf = std::collections::HashSet::new();
        a_dtmf.insert(110);
        let mut b_dtmf = std::collections::HashSet::new();
        b_dtmf.insert(110);

        let (a_to_b, b_to_a) = video_relay_rules(
            &a, &b, 1, 2, Some(96), a_dtmf, Some(96), b_dtmf, None, None,
        );

        let a_matched: Vec<u8> = a_to_b.iter().map(|r| r.match_payload_type.unwrap()).collect();
        let b_matched: Vec<u8> = b_to_a.iter().map(|r| r.match_payload_type.unwrap()).collect();
        // PT 96 (audio) and PT 110 (DTMF) must be excluded from the video rules.
        assert!(!a_matched.contains(&96) && !a_matched.contains(&110), "A video rules hijack audio/DTMF: {a_matched:?}");
        assert!(!b_matched.contains(&96) && !b_matched.contains(&110), "B video rules hijack audio/DTMF: {b_matched:?}");
    }
}
