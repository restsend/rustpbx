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
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::egress::EgressSource;
use crate::ingress_tap::{DtmfEvent, PacketDirection};
use crate::leg::Leg;
use crate::media_recorder::{
    FileRecorder, MediaRecorder, RecorderHandle, RecorderSender, RecorderStatus,
    RecordingCompletion,
};
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

/// Per-session 2-party media bridge.
pub struct MediaBridge {
    session_id: String,
    leg_a: Option<Leg>,
    leg_b: Option<Leg>,
    route_active: bool,
    /// Control half of the call-scoped recording task, installed only when
    /// recording setup is enabled for the caller-facing A leg.
    recorder_handle: Option<RecorderHandle>,
    recorder_finished_rx: Option<mpsc::UnboundedReceiver<RecordingCompletion>>,
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
    last_bridged: Option<(
        audio_codec::CodecType,
        audio_codec::CodecType,
        Option<(String, u8, String, u8)>,
    )>,
    /// Snapshot of the current legs, readable from the background 5s stats
    /// task without borrowing `&mut self`. Kept in sync by `replace_leg` /
    /// `teardown`.
    legs_shared: Arc<parking_lot::Mutex<(Option<Leg>, Option<Leg>)>>,
    /// Latch fired when a fast-path relay arming attempt fails (e.g. a WebRTC
    /// leg's DTLS/SRTP transport never became ready). The session monitors it
    /// via [`Self::relay_arm_failed_rx`] and re-bridges in transcode mode so
    /// the call keeps media. `watch` persists the latest value, so a
    /// notification that fires before the session starts waiting is never lost.
    relay_arm_failed: watch::Sender<bool>,
    /// Set once the bridge has fallen back to transcoding. Prevents `bridge()`
    /// from re-selecting the relay for the rest of the session (the relay
    /// already proved it cannot be armed).
    force_transcode: bool,
}

impl MediaBridge {
    pub fn new(session_id: impl Into<String>) -> Self {
        let (dtmf_bus, _) = broadcast::channel(8);
        let legs_shared = Arc::new(parking_lot::Mutex::new((None, None)));
        let cancel = CancellationToken::new();
        let session = session_id.into();
        crate::telemetry::MediaTelemetry::register_bridge();
        spawn_bridge_stats_task(
            session.clone(),
            Arc::clone(&legs_shared),
            cancel.child_token(),
        );
        let (relay_arm_failed, _) = watch::channel(false);
        Self {
            session_id: session,
            leg_a: None,
            leg_b: None,
            route_active: false,
            recorder_handle: None,
            recorder_finished_rx: None,
            dtmf_bus,
            root_cancel: cancel,
            leg_wire_cancels: HashMap::new(),
            rtcp_cancel: None,
            rtcp_forwarder_count: Arc::new(AtomicUsize::new(0)),
            active_play: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            last_bridged: None,
            legs_shared,
            relay_arm_failed,
            force_transcode: false,
        }
    }

    pub fn leg(&self, side: LegSide) -> Option<Leg> {
        match side {
            LegSide::A => self.leg_a.clone(),
            LegSide::B => self.leg_b.clone(),
        }
    }

    /// Create only the call-scoped capture task and return the sender that must
    /// be supplied while constructing the caller leg. Recorder implementation
    /// selection is a separate media-setup operation through `set_recorder`.
    pub fn setup_recorder_task(&mut self) -> Result<RecorderSender> {
        if self.recorder_handle.is_some() {
            return Err(anyhow!("recording task is already started"));
        }
        let (handle, sender, recorder_finished_rx) = RecorderHandle::new();
        self.recorder_handle = Some(handle);
        self.recorder_finished_rx = Some(recorder_finished_rx);
        Ok(sender)
    }

    /// Start file recording from caller leg A. The task initializes the file
    /// backend asynchronously before this resolves.
    pub async fn start_recording(
        &mut self,
        path: String,
        channels: u16,
        mono_caller_only: bool,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        let caller_profile = self
            .leg(LegSide::A)
            .and_then(|leg| leg.negotiated())
            .ok_or_else(|| anyhow!("no negotiated A-leg profile to record"))?;
        let recorder = FileRecorder::new(path, caller_profile, channels, mono_caller_only);
        self.set_recorder(Box::new(recorder), max_duration).await
    }

    /// Install and initialize the selected recorder implementation in the
    /// capture task that was prepared before caller-leg construction.
    pub async fn set_recorder(
        &mut self,
        recorder: Box<dyn MediaRecorder>,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .set_recorder(recorder, max_duration)
            .await
    }

    pub fn has_recorder_task(&self) -> bool {
        self.recorder_handle.is_some()
    }

    /// Whether the recording task currently owns an initialized recorder
    /// implementation.
    pub async fn has_recorder(&self) -> bool {
        self.recorder_status()
            .await
            .is_ok_and(|status| status.active)
    }

    pub async fn recorder_status(&self) -> Result<RecorderStatus> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .status()
            .await
    }

    pub fn pause_recording(&self) -> Result<()> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .pause()
    }

    pub fn resume_recording(&self) -> Result<()> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .resume()
    }

    /// Finalize only the current recorder. The call-scoped capture task stays
    /// alive and can accept another recorder later.
    pub async fn stop_recording(&mut self) -> RecordingCompletion {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .stop_recorder()
            .await
    }

    /// Wait for a recorder completion reported independently of a control
    /// command, such as max-duration expiry.
    pub async fn recv_recorder_finished(&mut self) -> Option<RecordingCompletion> {
        self.recorder_finished_rx.as_mut()?.recv().await
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
        if let Some(old) = self
            .leg_wire_cancels
            .insert(side, self.root_cancel.child_token())
        {
            old.cancel();
        }
        let cancel = self
            .leg_wire_cancels
            .get(&side)
            .cloned()
            .expect("just inserted");
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
        *self.legs_shared.lock() = (self.leg_a.clone(), self.leg_b.clone());
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

    // ── Routing ──────────────────────────────────────────────────────────

    /// Bridge A ↔ B. Both legs must exist and be answered (gate open). Selects
    /// [`EgressSource::RewriteRelay`] when the negotiated audio codecs match
    /// AND the legs share a video codec (or neither negotiated video);
    /// otherwise [`EgressSource::TranscodePeer`] (audio-only — video cannot be
    /// relayed without a common codec and is never transcoded). WebRTC legs
    /// are supported on the fast path: the rewrite bridge runs at the
    /// plaintext boundary, so SRTP is decrypted/encrypted per leg and
    /// matching codecs still relay zero-copy.
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

        // rustrtc's rewrite bridge forwards EVERY inbound packet — rules only
        // rewrite headers, there is no drop action — so with no common video
        // codec an unmatched video PT would fall through the audio catch-all
        // rule and leak to the peer in a codec it never negotiated (the peer
        // may even misroute it onto another track). When both legs negotiated
        // video but share no codec, degrade to the transcoding path: audio
        // still flows (decode → re-encode) and video stops entirely.
        let video_mismatch = !pa.video.is_empty() && !pb.video.is_empty() && video_match.is_none();
        let a_transport = la.pc().config().transport_mode.clone();
        let b_transport = lb.pc().config().transport_mode.clone();
        let has_webrtc_leg = a_transport == rustrtc::TransportMode::WebRtc
            || b_transport == rustrtc::TransportMode::WebRtc;

        // Idempotent re-bridge: same codec pair on an already-active route is
        // a no-op (avoid rebuilding decoders / re-arming the relay).
        let bridged_key = (
            ca.codec,
            cb.codec,
            video_match.as_ref().map(|(va, vb)| {
                (
                    va.name.clone(),
                    va.payload_type,
                    vb.name.clone(),
                    vb.payload_type,
                )
            }),
        );
        if self.route_active && self.last_bridged.as_ref() == Some(&bridged_key) {
            return Ok(());
        }
        self.last_bridged = Some(bridged_key);

        if video_mismatch {
            warn!(
                session = %self.session_id,
                a_video = ?pa.video.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
                b_video = ?pb.video.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
                "legs share no video codec — degrading to audio-only (transcoded)"
            );
        }

        if ca.codec == cb.codec && !video_mismatch && !self.force_transcode {
            // ── fast-path: transport-level zero-copy relay ──
            debug!(session = %self.session_id, codec = ?ca.codec, "MBRIDGE fast-path relay"); // Rewrite the forwarded packet's header to the destination leg's
            // negotiated SSRC / PT, and strip WebRTC extension headers when the
            // destination is plain RTP.
            //
            // Use the destination leg's outbound audio SSRC:
            // - WebRTC → paced sender / SDP `a=ssrc` (IVR + relay share it so
            //   browsers hear both local playback and bridged audio)
            // - plain RTP → distinct relay SSRC (isolates later local playback;
            //   RTP peers do not bind to SDP a=ssrc)
            let a_playback_ssrc = crate::leg::sender_ssrc_for_kind(la.pc(), MediaKind::Audio);
            let b_playback_ssrc = crate::leg::sender_ssrc_for_kind(lb.pc(), MediaKind::Audio);
            let a_out_ssrc = la.outbound_audio_ssrc();
            let b_out_ssrc = lb.outbound_audio_ssrc();
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

            // Build one logical rule table per direction. Leg arming installs
            // it once for a BUNDLE source, or partitions it across separate
            // audio/video source transports for non-BUNDLE.
            let mut rules_a_to_b = audio_relay_rules(
                b_out_ssrc,
                (ca.payload_type != cb.payload_type).then_some(cb.payload_type),
                dtmf_a_to_b,
                &b_audio_mid,
            );
            // ── B→A rules (mirror) ──
            let mut rules_b_to_a = audio_relay_rules(
                a_out_ssrc,
                (ca.payload_type != cb.payload_type).then_some(ca.payload_type),
                dtmf_b_to_a,
                &a_audio_mid,
            );
            let (video_a_to_b, video_b_to_a) = video_relay_rules(
                &pa.video,
                &pb.video,
                a_video_ssrc,
                b_video_ssrc,
                a_video_mid,
                b_video_mid,
            );
            let video_payload_types_a: Vec<u8> = video_a_to_b
                .iter()
                .filter_map(|rule| rule.match_payload_type)
                .collect();
            let video_payload_types_b: Vec<u8> = video_b_to_a
                .iter()
                .filter_map(|rule| rule.match_payload_type)
                .collect();
            rules_a_to_b.extend(video_a_to_b);
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
                a_playback_ssrc, b_playback_ssrc, a_out_ssrc, b_out_ssrc,
                video = ?video_match.as_ref().map(|(v, _)| v.name.as_str()),
                strip_a_to_b = options_a_to_b.strip_extensions,
                strip_b_to_a = options_b_to_a.strip_extensions,
                "fast-path relay selected; transport arming scheduled"
            );

            la.set_egress_source(EgressSource::RewriteRelay {
                peer_pc: lb.pc().clone(),
                options: options_a_to_b,
                rules: rules_a_to_b,
                video_payload_types: video_payload_types_a,
                on_arm_failed: Some(self.arm_failed_callback()),
            })
            .await?;
            lb.set_egress_source(EgressSource::RewriteRelay {
                peer_pc: la.pc().clone(),
                options: options_b_to_a,
                rules: rules_b_to_a,
                video_payload_types: video_payload_types_b,
                on_arm_failed: Some(self.arm_failed_callback()),
            })
            .await?;
            // Video receivers depend on RTCP PLI/FIR/NACK to recover lost video
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
            // Video PLI/FIR forwarding needs the peer's real ingress SSRC even
            // for RTP↔RTP. Audio-only RTP↔RTP can still skip this per-packet map.
            let needs_ssrc_pt_tracking = video_match.is_some() || has_webrtc_leg;
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
                primed: false,
            })
            .await?;
            lb.set_egress_source(EgressSource::TranscodePeer {
                peer: a_recv,
                decoder: a_decoder,
                src_sample_rate: a_src_rate,
                source_audio_payload_type: ca.payload_type,
                primed: false,
            })
            .await?;
            info!(
                session = %self.session_id,
                a_codec = ?ca.codec,
                b_codec = ?cb.codec,
                has_webrtc_leg,
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
            Box::new(crate::audio_source::FileAudioSource::new(path, loop_playback).await?),
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

    /// Resume from hold / play: re-activate the route (auto-selects
    /// fast-path or transcode). Clears any active-play markers for both legs.
    pub async fn resume(&mut self) -> Result<()> {
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
    /// sender feeds the egress pipeline via [`ChannelAudioSource`]. Underruns
    /// while the sender is alive keep RTP cadence with **digital silence**
    /// (not comfort-noise — CNG→speech transitions click every chunk). When
    /// the sender is dropped and the buffer drains, the source EOFs
    /// (`loop_playback=false`) so IVR return-app can start without an
    /// infinite CNG/silence tail.
    ///
    /// The source does NOT pre-encode — the leg's egress encoder converts
    /// PCM→codec at its own 20 ms cadence ("filetrack mode").
    ///
    /// `on_end` (if provided) fires when playback stops: `false` on natural
    /// EOF after the channel drains, `true` if interrupted.
    pub async fn bridge_play_pcm(
        &self,
        side: LegSide,
        sample_rate: u32,
        on_end: Option<crate::egress::EgressEndCallback>,
    ) -> Result<tokio::sync::mpsc::Sender<Vec<i16>>> {
        let leg = self
            .leg(side)
            .ok_or_else(|| anyhow!("no leg on {side:?}"))?;
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let source = Box::new(crate::audio_source::ChannelAudioSource::new(
            rx,
            sample_rate,
        ));
        leg.play(source, false, on_end).await?;
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

    /// Callback passed to `RewriteRelay` egress sources: fires the relay-arm
    /// failure latch when a WebRTC leg's transport never becomes ready.
    fn arm_failed_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let tx = self.relay_arm_failed.clone();
        Arc::new(move || {
            let _ = tx.send(true);
        })
    }

    /// A receiver that resolves when a fast-path relay arming attempt failed
    /// (latch value flips to `true`). The session monitors it from a spawned
    /// task and reacts with [`Self::force_transcode`]. `watch` persists the
    /// latest value, so a failure that already fired is not missed.
    pub fn relay_arm_failed_rx(&self) -> watch::Receiver<bool> {
        self.relay_arm_failed.subscribe()
    }

    /// Permanently force the transcode path for this session. The relay
    /// already failed to arm (e.g. WebRTC DTLS never came up), so re-selecting
    /// it would strand the call with no media. Clears the active route and
    /// re-bridges so the next activation picks transcoding.
    ///
    /// Idempotent: when transcode mode is already forced AND the forced
    /// route is active, repeat calls are a no-op — duplicate
    /// relay-arm-failure notifications must not re-run the unbridge/bridge
    /// cycle (each cycle tears media down and re-logs the activation).
    pub async fn force_transcode(&mut self) -> Result<()> {
        if self.force_transcode && self.route_active {
            return Ok(());
        }
        self.force_transcode = true;
        if self.route_active {
            self.unbridge().await?;
            self.bridge().await?;
        }
        Ok(())
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

    /// Per-leg media quality, captured at call end for the call record.
    /// Empty when no legs are present.
    pub fn quality_summary(&self) -> Vec<crate::leg_stats::LegQualityReport> {
        let mut out = Vec::new();
        for (side, leg) in [
            (LegSide::A, self.leg_a.as_ref()),
            (LegSide::B, self.leg_b.as_ref()),
        ] {
            let Some(leg) = leg else { continue };
            let tap = leg.stats();
            let rtcp = leg.rtcp_stats().snapshot();
            let codec = leg
                .negotiated()
                .and_then(|p| p.audio.as_ref().map(|c| format!("{:?}", c.codec)));
            out.push(crate::leg_stats::LegQualityReport {
                side: match side {
                    LegSide::A => "A",
                    LegSide::B => "B",
                },
                codec,
                ingress_packets: tap.ingress_packets,
                egress_packets: tap.egress_packets,
                transport_rx_packets: leg.pc().received_rtp_packets(),
                jitter_us: rtcp.jitter_us,
                rtt_us: rtcp.rtt_us,
                loss_pct: rtcp.loss_pct(),
            });
        }
        out
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
        crate::telemetry::MediaTelemetry::unregister_bridge();
        *self.legs_shared.lock() = (None, None);
        if let Some(la) = self.leg_a.take() {
            la.stop();
        }
        if let Some(lb) = self.leg_b.take() {
            lb.stop();
        }
        // Closing the last control handle makes the detached task drain any
        // queued RTP and finalize its current backend.
        self.recorder_handle.take();
    }
}

impl Drop for MediaBridge {
    fn drop(&mut self) {
        self.teardown();
    }
}

// ── Periodic per-bridge stats task ────────────────────────────────────────────

/// One leg's transport/tap/RTCP counters at a single sample point.
#[derive(Debug, Clone, Default)]
struct LegSample {
    ingress: u64,
    egress: u64,
    transport_rx: u64,
    sr_packet_count: u64,
    sr_ssrc: u32,
    has_sr: bool,
    jitter_us: u64,
    rtt_us: u64,
    fraction_lost: u8,
}

fn sample_leg(leg: &Leg) -> LegSample {
    let tap = leg.stats();
    let rtcp = leg.rtcp_stats().snapshot();
    LegSample {
        ingress: tap.ingress_packets,
        egress: tap.egress_packets,
        transport_rx: leg.pc().received_rtp_packets(),
        sr_packet_count: rtcp.sr_packet_count,
        sr_ssrc: rtcp.sr_ssrc,
        has_sr: rtcp.has_sr,
        jitter_us: rtcp.jitter_us,
        rtt_us: rtcp.rtt_us,
        fraction_lost: rtcp.fraction_lost,
    }
}

/// 5s window delta for one leg.
#[derive(Debug, Clone, Default)]
struct LegSampleDelta {
    ingress: u64,
    egress: u64,
    transport_rx: u64,
    sr: u64,
    jitter_us: u64,
    rtt_us: u64,
    fraction_lost: u8,
}

fn leg_delta(cur: Option<&LegSample>, prev: Option<&LegSample>) -> LegSampleDelta {
    let Some(cur) = cur else {
        return LegSampleDelta::default();
    };
    let prev = prev.cloned().unwrap_or_default();
    // SR packet count is only meaningful once a Sender Report has arrived.
    // If the remote SSRC changed (stream restart / audio↔video SR flip) the
    // cumulative counter resets, so treat the window as "fresh stream" instead
    // of computing a bogus huge delta.
    let sr = if cur.has_sr {
        if cur.sr_ssrc != 0 && prev.sr_ssrc != 0 && cur.sr_ssrc != prev.sr_ssrc {
            cur.sr_packet_count
        } else {
            cur.sr_packet_count.saturating_sub(prev.sr_packet_count)
        }
    } else {
        0
    };
    LegSampleDelta {
        ingress: cur.ingress.saturating_sub(prev.ingress),
        egress: cur.egress.saturating_sub(prev.egress),
        transport_rx: cur.transport_rx.saturating_sub(prev.transport_rx),
        sr,
        jitter_us: cur.jitter_us,
        rtt_us: cur.rtt_us,
        fraction_lost: cur.fraction_lost,
    }
}

fn fmt_ms(us: u64) -> String {
    if us == 0 {
        "-".to_string()
    } else {
        format!("{:.1}ms", us as f64 / 1000.0)
    }
}

/// Spawn the 5s media-quality sampler for a bridge. Publishes receive/send
/// deltas into the process-wide [`crate::telemetry::MediaTelemetry`] (feeding
/// the host's local stats log / Prometheus) and logs an `info!` line ONLY when
/// a quality anomaly is detected (internal drops or >=1% loss in either
/// direction), so idle bridges stay silent.
fn spawn_bridge_stats_task(
    session_id: String,
    legs_shared: Arc<parking_lot::Mutex<(Option<Leg>, Option<Leg>)>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // skip the immediate first tick
        let mut prev_a: Option<LegSample> = None;
        let mut prev_b: Option<LegSample> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let (la, lb) = {
                        let g = legs_shared.lock();
                        (g.0.clone(), g.1.clone())
                    };
                    let sa = la.as_ref().map(|l| sample_leg(l));
                    let sb = lb.as_ref().map(|l| sample_leg(l));

                    let da = leg_delta(sa.as_ref(), prev_a.as_ref());
                    let db = leg_delta(sb.as_ref(), prev_b.as_ref());

                    // Same-codec relay fast-path? Only then is "ingress on one
                    // leg minus egress on the peer" a clean internal-drop signal.
                    // In transcode mode the two legs run at different packet
                    // rates / ptime, so ingress-vs-egress is confounded by the
                    // codec rate difference and MUST NOT be reported as drops.
                    let relay_mode = match (la.as_ref(), lb.as_ref()) {
                        (Some(a), Some(b)) => match (a.negotiated(), b.negotiated()) {
                            (Some(pa), Some(pb)) => {
                                matches!((pa.audio.as_ref(), pb.audio.as_ref()),
                                    (Some(ca), Some(cb)) if ca.codec == cb.codec)
                            }
                            _ => false,
                        },
                        _ => false,
                    };

                    // ── rx bucket (system received) ──
                    let rx_packets_d = da.transport_rx + db.transport_rx;
                    let rx_expected_d = da.sr + db.sr;
                    let rx_lost_d = rx_expected_d.saturating_sub(rx_packets_d);
                    // Internal drops: received on one leg but not emitted on the
                    // peer leg (rustrtc mpsc/SPSC saturation, relay stall, ...).
                    // Relay-only (see `relay_mode`).
                    let rx_idrop_d = if relay_mode {
                        let dir_ab_drop = da.ingress.saturating_sub(db.egress);
                        let dir_ba_drop = db.ingress.saturating_sub(da.egress);
                        dir_ab_drop + dir_ba_drop
                    } else {
                        0
                    };

                    // ── tx bucket (system sent) ──
                    let tx_packets_d = da.egress + db.egress;
                    let tx_lost_d = (da.fraction_lost as f64 / 255.0 * da.egress as f64).round() as u64
                        + (db.fraction_lost as f64 / 255.0 * db.egress as f64).round() as u64;

                    let rx_loss_pct = if rx_expected_d > 0 {
                        rx_lost_d as f64 / rx_expected_d as f64 * 100.0
                    } else {
                        0.0
                    };
                    let tx_loss_pct = if tx_packets_d > 0 {
                        tx_lost_d as f64 / tx_packets_d as f64 * 100.0
                    } else {
                        0.0
                    };

                    crate::telemetry::MediaTelemetry::record_rx(rx_packets_d, rx_lost_d, rx_idrop_d);
                    crate::telemetry::MediaTelemetry::record_tx(tx_packets_d, tx_lost_d, 0);

                    let anomalous = rx_idrop_d > 0 || rx_loss_pct >= 1.0 || tx_loss_pct >= 1.0;
                    if anomalous && rx_packets_d + tx_packets_d > 0 {
                        info!(
                            bridge_id = %session_id,
                            relay = relay_mode,
                            a_ingress = da.ingress, a_egress = da.egress, a_rx = da.transport_rx,
                            a_jitter = fmt_ms(da.jitter_us), a_rtt = fmt_ms(da.rtt_us),
                            a_flost = format!("{:.1}%", da.fraction_lost as f64 / 255.0 * 100.0),
                            b_ingress = db.ingress, b_egress = db.egress, b_rx = db.transport_rx,
                            b_jitter = fmt_ms(db.jitter_us), b_rtt = fmt_ms(db.rtt_us),
                            b_flost = format!("{:.1}%", db.fraction_lost as f64 / 255.0 * 100.0),
                            rx_packets = rx_packets_d,
                            rx_loss = format!("{:.2}%", rx_loss_pct),
                            rx_idrop = rx_idrop_d,
                            tx_packets = tx_packets_d,
                            tx_loss = format!("{:.2}%", tx_loss_pct),
                            tx_idrop = 0u64,
                            "bridge media quality anomaly [5s]"
                        );
                    }

                    prev_a = sa;
                    prev_b = sb;
                }
            }
        }
    });
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

/// Rewrite rules for one direction of audio relay: the audio catch-all rule
/// (rewrites every packet to the destination leg's outbound audio SSRC, remapping the
/// payload type when the legs differ) plus, when the two legs negotiated
/// different telephone-event payload types, a DTMF remap rule. Both stamp the
/// destination leg's audio MID for browser attribution.
fn audio_relay_rules(
    out_ssrc: u32,
    out_pt: Option<u8>,
    dtmf_map: Option<(u8, u8)>,
    mid: &Option<(u8, std::sync::Arc<str>)>,
) -> Vec<RtpRewriteRule> {
    let (mid_id, mid_val) = mid
        .as_ref()
        .map(|m| (Some(m.0), Some(m.1.to_string())))
        .unwrap_or((None, None));
    let mut rules = vec![RtpRewriteRule {
        match_payload_type: None,
        fixed_out_ssrc: Some(out_ssrc),
        ssrc_offset: 0,
        out_payload_type: out_pt,
        sdes_mid_extension_id: mid_id,
        sdes_mid: mid_val.clone(),
    }];
    if let Some((src_pt, dst_pt)) = dtmf_map {
        rules.push(RtpRewriteRule {
            match_payload_type: Some(src_pt),
            fixed_out_ssrc: Some(out_ssrc),
            ssrc_offset: 0,
            out_payload_type: Some(dst_pt),
            sdes_mid_extension_id: mid_id,
            sdes_mid: mid_val,
        });
    }
    rules
}

/// Build the video payload-type rewrite rules for the fast-path relay.
///
/// The relay must match every supported video payload type a leg may actually send,
/// not just the first common profile. Each WebRTC peer picks its own send
/// profile (offerer vs answerer, hardware vs software encoder), so a leg may
/// send any negotiated H264 variant. An unmatched
/// video PT falls through to the audio catch-all rule and gets stamped with
/// the audio SSRC, which the peer drops: a persistent one-way video failure
/// (the peer sending on the covered PT still works, the other side is black).
///
/// For each video codec on leg A we add a rule matching that PT and rewriting
/// it to the peer leg's PT for the same (name, fmtp) codec, stamped with the
/// destination leg's video sender SSRC; the mirror covers B→A. BUNDLE payload
/// collisions are prevented while constructing the bundled leg's SDP, so this
/// function only maps the video codecs negotiated independently on each leg.
fn video_relay_rules(
    a: &[NegotiatedVideoCodec],
    b: &[NegotiatedVideoCodec],
    a_video_ssrc: u32,
    b_video_ssrc: u32,
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
            .or_else(|| {
                peer.iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&codec.name))
            })
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
fn leg_send_transport(
    leg: &Leg,
    kind: MediaKind,
) -> Option<Arc<rustrtc::transports::rtp::RtpTransport>> {
    leg.pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == kind)
        .and_then(|t| t.sender())
        .and_then(|s| s.transport())
}

/// Wire RTCP feedback relay for the fast-path RTP relay.
///
/// The RTP rewrite bridge forwards only RTP; each leg's RTCP (PLI/FIR/NACK) is
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
    let a_video_pts: Vec<u8> = pa.video.iter().map(|video| video.payload_type).collect();
    let b_video_pts: Vec<u8> = pb.video.iter().map(|video| video.payload_type).collect();
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
    wire_rtcp_sender_forward(
        b.clone(),
        a.clone(),
        MediaKind::Audio,
        a_audio_pts,
        cancel,
        forwarder_count,
    );
}

/// Spawn one RTCP-forwarding task: feedback (PLI/FIR/NACK) targeting a sender of
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
    let sender = src_leg
        .pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == kind)
        .and_then(|t| t.sender());
    let Some(sender) = sender else { return };
    let source_ssrc = sender.ssrc();
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
                    use rustrtc::rtp::{
                        FirRequest, FullIntraRequest, GenericNack, PictureLossIndication,
                        RtcpPacket,
                    };

                    let Ok(packet) = packet else { break };
                    // The peer's real sender SSRC for this media type; skip
                    // until the peer has actually started sending it.
                    let Some(target) = dst_tap.ingress_ssrc_for_pts(&dst_pts) else {
                        continue;
                    };
                    let forwarded = match packet {
                        RtcpPacket::PictureLossIndication(pli) => {
                            RtcpPacket::PictureLossIndication(PictureLossIndication {
                                sender_ssrc: pli.sender_ssrc,
                                media_ssrc: target,
                            })
                        }
                        RtcpPacket::FullIntraRequest(fir) => {
                            let Some(request) = fir
                                .requests
                                .iter()
                                .find(|request| request.ssrc == source_ssrc)
                            else {
                                continue;
                            };
                            RtcpPacket::FullIntraRequest(FullIntraRequest {
                                sender_ssrc: fir.sender_ssrc,
                                requests: vec![FirRequest {
                                    ssrc: target,
                                    sequence_number: request.sequence_number,
                                }],
                            })
                        }
                        RtcpPacket::GenericNack(nack) => {
                            RtcpPacket::GenericNack(GenericNack {
                                sender_ssrc: nack.sender_ssrc,
                                media_ssrc: target,
                                lost_packets: nack.lost_packets,
                            })
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leg::{LegConfig, LegInner};
    use crate::negotiate::CodecInfo;

    #[tokio::test]
    async fn set_legs_and_close() {
        let mut mb = MediaBridge::new("s1");
        let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        assert!(mb.leg(LegSide::A).is_some());
        assert!(mb.leg(LegSide::B).is_some());
        mb.close();
    }

    #[tokio::test]
    async fn dtmf_bus_forwards_ingress_and_ignores_egress() {
        use rustrtc::peer_connection::RtpObserver;
        use rustrtc::rtp::{RtpHeader, RtpPacket};

        let mut mb = MediaBridge::new("s4");
        let leg = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
        leg.ingress_tap().set_dtmf_payload_types(vec![101]);
        mb.replace_leg(LegSide::A, leg.clone()).await;
        let mut rx = mb.dtmf_bus();
        let addr: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();

        let egress = RtpPacket::new(RtpHeader::new(101, 1, 100, 1234), vec![1, 0x80, 0, 160]);
        leg.ingress_tap().on_egress(&egress, addr);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "egress DTMF must not be published to the session bus"
        );

        let ingress = RtpPacket::new(RtpHeader::new(101, 2, 200, 1234), vec![2, 0x80, 0, 160]);
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
        let mut mb = MediaBridge::new("s5");
        let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;
        mb.close();
    }

    #[tokio::test]
    async fn rtp_legs_negotiate_and_bridge_fastpath() {
        use rustrtc::SdpType;

        // Two RTP legs negotiate UAC/UAS-style SDP with each other (no DTLS),
        // then the bridge activates the same-codec fast-path relay.
        let mut mb = MediaBridge::new("s6");
        let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();

        let a_offer = a.create_offer().await.expect("a offer");
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

        // Two WebRTC (DTLS-SRTP) legs negotiate UAC/UAS-style SDP with each
        // other. Same codec (opus) → fast-path relay on both legs.
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
        let mut mb = MediaBridge::new("s7");
        let a = LegInner::new("a", &cfg, None).unwrap();
        let b = LegInner::new("b", &cfg, None).unwrap();

        let a_offer = a.create_offer().await.expect("a offer");
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
        for side in [LegSide::A, LegSide::B] {
            assert!(
                mb.leg(side).expect("leg").egress_is_relay(),
                "WebRTC leg {side:?} should use fast-path relay"
            );
        }
        mb.close();
    }

    /// WebRTC legs with matching audio+video codecs relay on the fast path
    /// (audio + video rewritten at transport level, RTCP PLI/NACK relayed).
    /// In the unit-test environment the DTLS transports never come up, so the
    /// RTCP-forwarding tasks must degrade gracefully (no panic, no spin) while
    /// the RTP relay rules are still installed and both legs report fast-path.
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
            video_codecs: crate::negotiate::tests::test_video_codecs(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("webrtc-video".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };
        let mut mb = MediaBridge::new("s-video-rtcp");
        let a = LegInner::new("a", &cfg, None).unwrap();
        let b = LegInner::new("b", &cfg, None).unwrap();

        let a_offer = a.create_offer().await.expect("a offer");
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
            assert!(
                leg.egress_is_relay(),
                "WebRTC leg {side:?} should use fast-path relay"
            );
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

        let a = LegInner::new("a", &webrtc_cfg, None).unwrap();
        let a2 = LegInner::new("a2", &webrtc_cfg, None).unwrap();
        let a_offer = a.create_offer().await.expect("a offer");
        let a2_answer = a2
            .apply_sdp(&a_offer, SdpType::Offer)
            .await
            .expect("a2 answers a");
        a.apply_sdp(&a2_answer, SdpType::Answer)
            .await
            .expect("a applies answer");

        let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
        let b2 = LegInner::new("b2", &LegConfig::rtp_pcmu(), None).unwrap();
        let b_offer = b.create_offer().await.expect("b offer");
        let b2_answer = b2
            .apply_sdp(&b_offer, SdpType::Offer)
            .await
            .expect("b2 answers b");
        b.apply_sdp(&b2_answer, SdpType::Answer)
            .await
            .expect("b applies answer");

        assert!(a.negotiated().is_some());
        assert!(b.negotiated().is_some());

        let mut mb = MediaBridge::new("s8");
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

    /// WebRTC Opus ↔ plain-RTP Opus uses the same-codec fast-path rewrite relay
    /// (with `strip_extensions` toward the RTP leg).
    ///
    /// Dual Opus PeerConnections need a larger stack than the default test thread.
    #[test]
    fn webrtc_rtp_opus_cross_transport_uses_fast_path() {
        let handle = std::thread::Builder::new()
            .name("opus-xport-test".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
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
                        cname: Some("x-opus-webrtc".to_string()),
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
                        cname: Some("x-opus-rtp".to_string()),
                        comfort_noise: true,
                        comfort_noise_level_db: -35.0,
                    };

                    let a = LegInner::new("a", &webrtc_cfg, None).unwrap();
                    let a2 = LegInner::new("a2", &webrtc_cfg, None).unwrap();
                    let a_offer = a.create_offer().await.expect("a offer");
                    let a2_answer = a2
                        .apply_sdp(&a_offer, SdpType::Offer)
                        .await
                        .expect("a2 answers a");
                    a.apply_sdp(&a2_answer, SdpType::Answer)
                        .await
                        .expect("a applies answer");
                    drop(a2);

                    let b = LegInner::new("b", &rtp_opus_cfg, None).unwrap();
                    let b2 = LegInner::new("b2", &rtp_opus_cfg, None).unwrap();
                    let b_offer = b.create_offer().await.expect("b offer");
                    let b2_answer = b2
                        .apply_sdp(&b_offer, SdpType::Offer)
                        .await
                        .expect("b2 answers b");
                    b.apply_sdp(&b2_answer, SdpType::Answer)
                        .await
                        .expect("b applies answer");
                    drop(b2);

                    let mut mb = MediaBridge::new("s-opus-xport");
                    mb.replace_leg(LegSide::A, a).await;
                    mb.replace_leg(LegSide::B, b).await;
                    mb.accept(LegSide::A).await;
                    mb.accept(LegSide::B).await;

                    assert!(mb.is_bridged());
                    assert!(
                        mb.leg(LegSide::A).unwrap().egress_is_relay(),
                        "WebRTC↔RTP Opus should use RewriteRelay"
                    );
                    assert!(
                        mb.leg(LegSide::B).unwrap().egress_is_relay(),
                        "leg B should use RewriteRelay"
                    );
                    mb.close();
                });
            })
            .expect("spawn");
        handle.join().expect("join");
    }

    /// A same-codec WebRTC↔RTP PCMU bridge uses the fast-path relay and must
    /// not wait for the WebRTC DTLS/SRTP transport before call setup can continue
    /// (the deferred-arming path arms the rewrite bridge in the background).
    /// Opus cross-transport also uses this fast path (see
    /// `webrtc_rtp_opus_cross_transport_uses_fast_path`).
    #[tokio::test]
    async fn fastpath_relay_does_not_block_on_unready_webrtc_transport() {
        use rustrtc::SdpType;

        let webrtc_cfg = LegConfig {
            transport: rustrtc::TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 0,
                codec: audio_codec::CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
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
        let rtp_pcmu_cfg = LegConfig {
            transport: rustrtc::TransportMode::Rtp,
            codecs: vec![CodecInfo {
                payload_type: 0,
                codec: audio_codec::CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
            }],
            video_codecs: Vec::new(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("rtp-pcmu".to_string()),
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
            m=audio 5000 UDP/TLS/RTP/SAVPF 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n\
            a=setup:actpass\r\n\
            a=ice-ufrag:uv50\r\n\
            a=ice-pwd:ib8b\r\n\
            a=candidate:1 1 udp 2130706431 10.0.0.1 5000 typ host\r\n";
        let a = LegInner::new("a", &webrtc_cfg, None).unwrap();
        a.apply_sdp(webrtc_offer, SdpType::Offer)
            .await
            .expect("a answers webrtc offer");
        assert!(
            a.negotiated().is_some(),
            "leg A profile should be negotiated"
        );

        // Leg B: RTP/PCMU — negotiated, transport ready.
        let b2 = LegInner::new("b2", &rtp_pcmu_cfg, None).unwrap();
        let b_offer = b2.create_offer().await.expect("b2 offer");
        let b = LegInner::new("b", &rtp_pcmu_cfg, None).unwrap();
        b.apply_sdp(&b_offer, SdpType::Offer)
            .await
            .expect("b answers rtp offer");
        assert!(
            b.negotiated().is_some(),
            "leg B profile should be negotiated"
        );

        let mut mb = MediaBridge::new("s-no-deadlock");
        mb.replace_leg(LegSide::A, a).await;
        mb.replace_leg(LegSide::B, b).await;

        // Same codec on both legs → fast-path relay, with arming deferred to a
        // background task so accept never blocks on the unready WebRTC peer.
        let start = std::time::Instant::now();
        mb.accept(LegSide::A).await;
        mb.accept(LegSide::B).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "accept must not block on the unready WebRTC transport (took {elapsed:?})"
        );
        assert!(mb.is_bridged(), "route should be active after both answer");
        for side in [LegSide::A, LegSide::B] {
            assert!(
                mb.leg(side).unwrap().egress_is_relay(),
                "leg {side:?} should select the fast-path relay (armed in background)"
            );
        }

        // The WebRTC peer never connects → the deferred arming fails and the
        // bridge must surface it so the session can fall back to transcoding.
        let fallback = mb.relay_arm_failed_rx();
        let fired = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            let mut rx = fallback;
            loop {
                if *rx.borrow_and_update() {
                    break;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        assert!(
            fired.is_ok(),
            "relay arming failure must be signaled for fallback to transcode"
        );

        // Fallback must switch the session to transcode (relay un-selected).
        mb.force_transcode().await.expect("force transcode");
        for side in [LegSide::A, LegSide::B] {
            assert!(
                !mb.leg(side).unwrap().egress_is_relay(),
                "leg {side:?} should be on transcode after relay-arm fallback"
            );
        }

        // Duplicate relay-arm-failure notifications (e.g. from two monitors
        // subscribed to the same latch) must NOT re-run the unbridge/bridge
        // cycle: force_transcode is idempotent while the forced route is
        // active.
        let was_bridged = mb.is_bridged();
        mb.force_transcode().await.expect("repeat force transcode");
        assert_eq!(
            mb.is_bridged(),
            was_bridged,
            "repeat force_transcode must keep the route active and unchanged"
        );
        for side in [LegSide::A, LegSide::B] {
            assert!(
                !mb.leg(side).unwrap().egress_is_relay(),
                "leg {side:?} must stay on transcode after repeat force"
            );
        }
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

        let mut mb = MediaBridge::new("pcm-stream");
        let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
        let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();

        let a_offer = a.create_offer().await.expect("a offer");
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
            rtcp_fbs: vec![],
            rtx_payload_type: None,
        }
    }

    /// The fast-path relay must install a rewrite rule for EVERY negotiated
    /// video payload type, not just the first common codec. A browser may send
    /// video on any negotiated H264 profile; an unmatched
    /// PT falls to the audio catch-all, is stamped with the AUDIO SSRC, and is
    /// dropped by the peer — the one-way video failure.
    #[test]
    fn video_relay_rules_cover_all_negotiated_video_pts() {
        // Real browser negotiation (mirrors the H264 subset of a Chrome offer
        // → bridge answer) with several profiles at preserved PTs (96..=124).
        let a = vec![
            vcap(
                "H264",
                96,
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"),
            ),
            vcap(
                "H264",
                104,
                Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"),
            ),
            vcap(
                "H264",
                108,
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
            ),
            vcap(
                "H264",
                114,
                Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"),
            ),
            vcap(
                "H264",
                116,
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f"),
            ),
            vcap(
                "H264",
                39,
                Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=4d001f"),
            ),
            vcap(
                "H264",
                118,
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64001f"),
            ),
        ];
        let b = a.clone();

        let (a_to_b, b_to_a) = video_relay_rules(
            &a, &b, 0xA0A0A0A0, // a_video_ssrc
            0xB0B0B0B0, // b_video_ssrc
            None, None,
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
            let rule = a_to_b_by_pt
                .get(&va.payload_type)
                .unwrap_or_else(|| panic!("no A→B relay rule for video PT {}", va.payload_type));
            assert_eq!(
                rule.fixed_out_ssrc,
                Some(0xB0B0B0B0),
                "A→B must use B's video SSRC"
            );
            assert_eq!(
                rule.out_payload_type,
                Some(va.payload_type),
                "PT preserved across legs"
            );
        }
        for vb in &b {
            let rule = b_to_a_by_pt
                .get(&vb.payload_type)
                .unwrap_or_else(|| panic!("no B→A relay rule for video PT {}", vb.payload_type));
            assert_eq!(
                rule.fixed_out_ssrc,
                Some(0xA0A0A0A0),
                "B→A must use A's video SSRC"
            );
            assert_eq!(
                rule.out_payload_type,
                Some(vb.payload_type),
                "PT preserved across legs"
            );
        }
    }

    #[test]
    fn video_relay_rules_support_vp8_payload_rewrite() {
        let a = vec![vcap("VP8", 96, None)];
        let b = vec![vcap("vp8", 110, None)];

        let (a_to_b, b_to_a) = video_relay_rules(&a, &b, 0xA0A0A0A0, 0xB0B0B0B0, None, None);

        assert_eq!(a_to_b.len(), 1);
        assert_eq!(a_to_b[0].match_payload_type, Some(96));
        assert_eq!(a_to_b[0].out_payload_type, Some(110));
        assert_eq!(a_to_b[0].fixed_out_ssrc, Some(0xB0B0B0B0));
        assert_eq!(b_to_a.len(), 1);
        assert_eq!(b_to_a[0].match_payload_type, Some(110));
        assert_eq!(b_to_a[0].out_payload_type, Some(96));
        assert_eq!(b_to_a[0].fixed_out_ssrc, Some(0xA0A0A0A0));
    }

    #[test]
    fn video_relay_rules_map_reused_rtp_pt_to_bundled_pt() {
        let rtp_video = vec![vcap("H264", 96, Some("profile-level-id=42801F"))];
        let webrtc_video = vec![vcap("H264", 97, Some("profile-level-id=42801F"))];

        let (rtp_to_webrtc, webrtc_to_rtp) =
            video_relay_rules(&rtp_video, &webrtc_video, 1, 2, None, None);

        assert_eq!(rtp_to_webrtc.len(), 1);
        assert_eq!(rtp_to_webrtc[0].match_payload_type, Some(96));
        assert_eq!(rtp_to_webrtc[0].out_payload_type, Some(97));
        assert_eq!(webrtc_to_rtp.len(), 1);
        assert_eq!(webrtc_to_rtp[0].match_payload_type, Some(97));
        assert_eq!(webrtc_to_rtp[0].out_payload_type, Some(96));
    }
}
