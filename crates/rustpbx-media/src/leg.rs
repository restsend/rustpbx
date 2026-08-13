//! Per-leg media: a PeerConnection that is never replaced, wired to an
//! [`IngressTap`] (plaintext bidirectional observation) and an
//! [`EgressPipeline`] (ptime-paced outbound frames).
//!
//! A [`Leg`] is the atomic unit owned by a `MediaBridge`. Its lifetime equals
//! the session's: re-INVITEs update SDP / codec in place (never recreate the
//! PC), which is the root fix for the "restart loses wiring" bug class.
//!
//! [`Leg`] is `Arc<LegInner>` — cheaply cloneable, so callers never hold
//! `Arc<Leg>`.
//!
//! ## Egress dispatch
//!
//! [`LegInner::set_egress_source`] routes [`EgressSource::RewriteRelay`] to the
//! PC's rewrite bridge (transport-level, ICE exclusively owned) and all other
//! sources to the always-alive [`EgressPipeline`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use parking_lot::Mutex;
use rustrtc::{
    PeerConnection, RtcConfiguration, RtpCodecParameters, SdpType, SessionDescription,
    TransportMode,
    config::BufferDropStrategy,
    media::MediaKind,
    media::track::sample_track,
    rtp::{RtpHeader, RtpPacket},
};
use tokio::sync::{broadcast, oneshot};
use tracing::debug;

use crate::egress::{EgressCodec, EgressPipeline, EgressSource};
use crate::ingress_tap::{DtmfEvent, IngressTap, MediaRecorder, TapStats};
use crate::leg_id::LegId;
use crate::negotiate::{self, CodecInfo, NegotiatedLegProfile};

/// RFC 4733 telephone-event duration for a 20 ms event at the given clock
/// rate (e.g. 160 @ 8 kHz, 960 @ 48 kHz).
fn dtmf_event_duration_for_clock(clock_rate: u32) -> u16 {
    ((clock_rate * 20) / 1000) as u16
}

/// Configuration for creating a [`Leg`]'s PeerConnection.
#[derive(Clone)]
pub struct LegConfig {
    pub transport: TransportMode,
    /// Codec preference (first entry is offered/used for the egress sender).
    pub codecs: Vec<CodecInfo>,
    /// Video capabilities advertised on this leg's video m-line. Empty when the
    /// leg carries no video (audio-only call).
    pub video_codecs: Vec<rustrtc::config::VideoCapability>,
    pub rtp_port_range: Option<(u16, u16)>,
    pub external_ip: Option<String>,
    pub bind_ip: Option<String>,
    pub cname: Option<String>,
    /// Emit comfort noise (instead of digital silence) when the leg's egress
    /// has no source. Defaults to true.
    pub comfort_noise: bool,
    /// Comfort-noise level in dBFS. Ignored when `comfort_noise` is false.
    pub comfort_noise_level_db: f32,
}

impl LegConfig {
    /// Minimal RTP/PCMU config (handy for tests).
    pub fn rtp_pcmu() -> Self {
        Self {
            transport: TransportMode::Rtp,
            codecs: vec![CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
            }],
            video_codecs: Vec::new(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: None,
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        }
    }
}

/// Inner (data) half of a [`Leg`]. See the module docs.
pub struct LegInner {
    id: LegId,
    pc: PeerConnection,
    tap: Arc<IngressTap>,
    /// Egress pipeline — always alive; the source is switched via the command
    /// channel. For [`EgressSource::RewriteRelay`] the pacing loop parks.
    egress: EgressPipeline,
    /// Whether the current egress source is [`EgressSource::RewriteRelay`].
    /// Tracked so the Leg knows when to tear down/rebuild the PC's rewrite
    /// bridge on mode switches (EgressSource is not Clone).
    was_relay: AtomicBool,
    negotiated: Mutex<Option<NegotiatedLegProfile>>,
    /// Gate: before the remote peer answers (200 OK), relay must not forward.
    /// Set true on construction, flipped to false by [`LegInner::accept`].
    gated: Arc<AtomicBool>,
    /// RTP inactivity timeout state — armed by the session, monitored by the
    /// per-leg DTMF forward task (no dedicated spawn).
    rtp_timeout: Arc<RtpTimeoutState>,
    /// True once the ingress tap has been attached to the (now-created) RTP
    /// transport. The transport does not exist at construction, so the tap is
    /// (re)attached after the first SDP application.
    observer_attached: AtomicBool,
    /// Handle of the background observer-attach task, so `stop` can abort it
    /// (prevents a leaked task waiting on a transport that never appears).
    observer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Handle of the deferred fast-path relay-arming task (spawned when a
    /// WebRTC peer's SRTP transport is not ready yet). Aborted on `stop` /
    /// `Drop` and before re-spawning, so stale arming tasks never pile up
    /// across negotiation churn / leg replacement.
    relay_arm_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Outbound DTMF (RFC 2833) send state: the RTP payload type used for
    /// telephone-events and the next sequence number / timestamp. Only valid
    /// after a profile is negotiated (`dtmf_pt` set); until then `send_dtmf`
    /// is a no-op.
    dtmf_send: parking_lot::Mutex<DtmfSendState>,
    /// Comfort-noise settings, preserved so `update_codec` (re-INVITE codec
    /// switch) rebuilds the egress codec with the same CNG behaviour.
    comfort_noise: bool,
    comfort_noise_level_db: f32,
}

/// Outbound RFC 2833 telephone-event send state.
#[derive(Clone, Copy, Default)]
struct DtmfSendState {
    /// Negotiated telephone-event payload type (e.g. 101), or `None` if the
    /// leg did not negotiate one (or has no profile yet).
    dtmf_pt: Option<u8>,
    /// Negotiated telephone-event clock rate (RFC 4733: the audio codec's
    /// clock rate, e.g. 48000 for opus, 8000 for PCMU/G722). Defaults to
    /// 8000 when no profile is applied yet.
    dtmf_clock_rate: u32,
    /// Next sequence number for outbound telephone-event packets.
    sequence: u16,
    /// Timestamp base. Advanced by the event duration for each digit so
    /// successive digits do not collide.
    timestamp: u32,
}

/// Shared RTP inactivity timeout state. The monitor task only polls while a
/// timeout is armed (`armed_at.is_some()`); arming/resuming wakes it via
/// [`RtpTimeoutState::armed`]. The session toggles the fields via
/// [`LegInner::arm_rtp_timeout`] etc.
pub struct RtpTimeoutState {
    /// Whether the timeout is currently armed and counting.
    pub active: AtomicBool,
    /// App-level suppression flag: when set, the monitor never fires even if
    /// `active` is true. Set by the session while an app (IVR/voicemail/queue)
    /// drives the call or during a blind transfer — periods where a leg may
    /// legitimately stay silent. Independent of `active` so `play()`'s
    /// pause/resume (which toggles `active`) cannot re-arm a suppressed timer.
    pub app_paused: AtomicBool,
    /// Timeout duration in milliseconds.
    pub duration_ms: AtomicU64,
    /// When the current countdown started (arm / last packet / resume).
    /// `None` when not armed.
    pub armed_at: Mutex<Option<Instant>>,
    /// Fire channel: sent once when no ingress packets arrive within the
    /// duration. Dropped on disarm → the receiver gets `Err(Canceled)`.
    pub fire_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Wakes the wire_leg monitor when a timeout is armed/resumed, so it
    /// doesn't poll a 100ms interval for legs that never arm one.
    pub armed: tokio::sync::Notify,
}

impl Default for RtpTimeoutState {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            app_paused: AtomicBool::new(false),
            duration_ms: AtomicU64::new(0),
            armed_at: Mutex::new(None),
            fire_tx: Mutex::new(None),
            armed: tokio::sync::Notify::new(),
        }
    }
}

/// A single media leg. Cheaply cloneable (`Arc<LegInner>`).
pub type Leg = Arc<LegInner>;

impl LegInner {
    /// Create a leg from a pre-built [`RtcConfiguration`] (so callers can
    /// reuse their existing dialplan/transport config logic verbatim) and the
    /// codec list for the egress sender. Installs the ingress tap as an
    /// observer and starts the egress pipeline in silence.
    pub fn from_rtc_config(
        id: impl Into<String>,
        mut rtc_config: RtcConfiguration,
        codecs: Vec<CodecInfo>,
        comfort_noise: bool,
        comfort_noise_level_db: f32,
    ) -> Result<Leg> {
        let first_codec = codecs.first().ok_or_else(|| anyhow!("no codecs"))?;

        // Stamp the correlation label (e.g. rustpbx's `{session_id}-{leg}`) on
        // the RtcConfiguration so rustrtc's per-PC tracing span carries it —
        // this is what groups every rustrtc log for this leg's connection.
        let label: String = id.into();
        if rtc_config.label.is_none() {
            rtc_config.label = Some(label.clone());
        }

        // Video capabilities advertised on this leg's video m-line (empty when
        // the leg carries no video). Read before `rtc_config` is moved into
        // `PeerConnection::new`.
        let video_caps = rtc_config
            .media_capabilities
            .as_ref()
            .map(|caps| caps.video.clone())
            .unwrap_or_default();

        let pc = {
            let handle = tokio::runtime::Handle::try_current();
            let _guard = handle.as_ref().ok().map(|h| h.enter());
            PeerConnection::new(rtc_config)
        };

        // Egress audio track: create the push/source pair and add the track to
        // the PC so it appears in the SDP.
        //
        // The ring capacity only needs to absorb producer/consumer jitter: the
        // egress pacing task pushes exactly one frame per ptime (20 ms) and the
        // packetizer drains immediately, so a handful of slots is plenty. The
        // original 500 pre-allocated ~95 KB of `MaybeUninit<MediaSample>` slots
        // per leg (drop-oldest semantics mean depth never meaningfully exceeds
        // 1-2), which at 1600 legs ≈ 150 MB of wasted reserved memory.
        let (sender, track, _feedback) = sample_track(MediaKind::Audio, 8);
        let params = RtpCodecParameters {
            payload_type: first_codec.payload_type,
            clock_rate: first_codec.clock_rate,
            channels: first_codec.channels as u8,
        };
        let _ = pc.add_track(track, params);

        // Egress video track: add ONE video sender when the config advertises
        // video capabilities. Its presence makes rustrtc emit `a=ssrc` on the
        // video m-line (fixing the browser's 2–3 s unsignaled-SSRC demux delay)
        // and provides the video sender SSRC used as the relay destination.
        // The sender's source stays idle — relayed video bypasses it entirely.
        if let Some(first_video) = video_caps.first() {
            let (_, video_track, _) = sample_track(MediaKind::Video, 8);
            let video_params = RtpCodecParameters {
                payload_type: first_video.payload_type,
                clock_rate: first_video.clock_rate,
                channels: 0,
            };
            let _ = pc.add_track(video_track, video_params);
        }

        // Plaintext bidirectional observer (stats / DTMF / recording).
        // 8 DTMF-event slots is plenty: digits are rare and lagged receivers
        // are dropped, never blocking the hot path.
        let tap = IngressTap::new(8);
        pc.add_observer(tap.clone());

        // Gate: closed until the call is answered (`accept`). The egress
        // pipeline parks on it so the leg never emits audio (even silence) to
        // its remote peer before the answer; early-media playback switches the
        // source to Media which still produces frames.
        let gated = Arc::new(AtomicBool::new(true));

        // Egress pipeline: ptime-paced push into the sender. Silence until the
        // caller switches the source (e.g. via play/hold).
        let dtmf_payload_type = codecs
            .iter()
            .find(|codec| {
                codec.codec == CodecType::TelephoneEvent
                    && codec.clock_rate == first_codec.clock_rate
            })
            .map(|codec| codec.payload_type);
        let egress_codec = EgressCodec {
            codec: first_codec.codec,
            payload_type: first_codec.payload_type,
            clock_rate: first_codec.clock_rate,
            dtmf_payload_type,
            comfort_noise,
            comfort_noise_level_db,
        };
        let egress = EgressPipeline::start_with_gate(
            sender,
            egress_codec,
            EgressSource::Silence,
            None,
            Some(gated.clone()),
        );

        Ok(Arc::new(LegInner {
            id: LegId::from(label),
            pc,
            tap,
            egress,
            was_relay: AtomicBool::new(false),
            negotiated: Mutex::new(None),
            gated: Arc::new(AtomicBool::new(true)),
            rtp_timeout: Arc::new(RtpTimeoutState::default()),
            observer_attached: AtomicBool::new(false),
            observer_task: Mutex::new(None),
            relay_arm_task: Mutex::new(None),
            dtmf_send: parking_lot::Mutex::new(DtmfSendState::default()),
            comfort_noise,
            comfort_noise_level_db,
        }))
    }

    /// Attach the ingress tap observer to the RTP transport(s). The transport
    /// is created lazily (async, after ICE connects) following the first
    /// `set_remote_description`, so the observer registered in
    /// `from_rtc_config` is a no-op until then. Call this after SDP is applied.
    /// Spawns a background task so it never blocks SDP negotiation. The task
    /// handle is stored so `stop` can abort it; a transport that never becomes
    /// ready logs a warning instead of silently dropping stats/DTMF/recording.
    fn ensure_observer(&self) {
        if self.observer_attached.swap(true, Ordering::SeqCst) {
            return;
        }
        let pc = self.pc.clone();
        let tap = self.tap.clone();
        let handle = tokio::spawn(async move {
            if pc
                .wait_for_rtp_transport_ready(std::time::Duration::from_secs(5))
                .await
                .is_err()
            {
                tracing::warn!(
                    "RTP transport never became ready; ingress tap NOT attached (no stats/DTMF/recording for this leg)"
                );
                return;
            }
            pc.add_observer(tap);
        });
        *self.observer_task.lock() = Some(handle);
    }

    /// Create a leg from a simplified [`LegConfig`] (builds the
    /// [`RtcConfiguration`] internally). Prefer [`Self::from_rtc_config`] when
    /// you already have a fully-configured `RtcConfiguration`.
    pub fn new(id: impl Into<String>, cfg: &LegConfig) -> Result<Leg> {
        Self::from_rtc_config(
            id,
            build_rtc_config(cfg),
            cfg.codecs.clone(),
            cfg.comfort_noise,
            cfg.comfort_noise_level_db,
        )
    }

    pub fn id(&self) -> &LegId {
        &self.id
    }

    pub fn pc(&self) -> &PeerConnection {
        &self.pc
    }

    pub fn ingress_tap(&self) -> &Arc<IngressTap> {
        &self.tap
    }

    /// The negotiated leg profile, populated after the first successful SDP
    /// answer is applied.
    pub fn negotiated(&self) -> Option<NegotiatedLegProfile> {
        self.negotiated.lock().clone()
    }

    pub fn stats(&self) -> TapStats {
        self.tap.stats()
    }

    pub fn subscribe_dtmf(&self) -> broadcast::Receiver<DtmfEvent> {
        self.tap.subscribe_dtmf()
    }

    pub fn set_recorder(&self, recorder: Option<Arc<dyn MediaRecorder>>) {
        self.tap.set_recorder(recorder);
    }

    /// Mark the leg as answered (remote peer accepted the call). Opens the
    /// relay gate so [`MediaBridge`] can activate the rewrite bridge.
    pub fn accept(&self) {
        self.gated.store(false, Ordering::Release);
    }

    /// Whether the leg has been answered yet.
    pub fn is_gated(&self) -> bool {
        self.gated.load(Ordering::Acquire)
    }

    // ── SDP ──────────────────────────────────────────────────────────────

    /// Create an offer SDP (as UAC). `prefer` reorders the codec preference
    /// (use the peer leg's codecs to maximize same-codec relay).
    pub async fn create_offer(&self, _prefer: Vec<CodecType>) -> Result<String> {
        // rustrtc gathering pattern: first create_offer primes ICE gathering,
        // wait for candidates, then the second call includes them in the SDP.
        // RTP mode returns instantly; WebRTC/SRTP need the wait.
        let _ = self.pc.create_offer().await?;
        self.pc.wait_for_gathering_complete().await;
        let offer = self.pc.create_offer().await?;
        let sdp = set_local(&self.pc, offer)?;
        debug!(
            leg = %self.id,
            sdp = ?sdp,
            "leg SDP offer created",
        );
        Ok(sdp)
    }

    /// Apply a remote SDP (offer or answer) and return the local SDP to send
    /// back (an answer when `sdp_type == Offer`, or empty when applying an
    /// answer as UAC). Also extracts and stores the negotiated profile and
    /// refreshes the egress codec + DTMF payload types.
    pub async fn apply_sdp(&self, remote: &str, sdp_type: SdpType) -> Result<String> {
        debug!(
            leg = %self.id,
            sdp_type = ?sdp_type,
            sdp = ?remote,
            "leg SDP applied",
        );
        let desc = SessionDescription::parse(sdp_type, remote)
            .map_err(|e| anyhow!("failed to parse remote sdp: {:?}", e))?;
        self.pc
            .set_remote_description(desc)
            .await
            .map_err(|e| anyhow!("set_remote_description failed: {}", e))?;
        // The RTP transport is created inside set_remote_description; the tap
        // observer registered at construction is a no-op until then, so attach
        // it now that the transport exists.
        self.ensure_observer();

        let local_sdp = if sdp_type == SdpType::Offer {
            // rustrtc gathering pattern: the first create_answer primes ICE
            // gathering; wait for candidates, then the second includes them.
            // RTP mode returns instantly; WebRTC/SRTP need the wait.
            let _ = self.pc.create_answer().await?;
            self.pc.wait_for_gathering_complete().await;
            let mut answer = self.pc.create_answer().await?;
            answer.sdp_type = SdpType::Answer;
            set_local(&self.pc, answer)?
        } else {
            // Applying a remote answer (UAC): no local SDP to emit.
            String::new()
        };

        // R1 fix: sync the sender's codec/PT to the negotiated answer.
        // Extract the profile from the answer SDP: the local answer we just
        // produced (as UAS), or the remote answer we applied (as UAC).
        let profile_sdp = if local_sdp.is_empty() {
            remote
        } else {
            local_sdp.as_str()
        };
        let profile = negotiate::MediaNegotiator::extract_leg_profile(profile_sdp);

        // Keep the destination audio and telephone-event payload types in sync
        // with the negotiated answer. The DTMF clock follows the audio clock.
        if let Some(audio) = &profile.audio {
            let dtmf_payload_type = profile.dtmf.as_ref().map(|dtmf| dtmf.payload_type);
            let previous = self.negotiated.lock().as_ref().and_then(|profile| {
                profile.audio.as_ref().map(|audio| {
                    (
                        audio.codec,
                        audio.payload_type,
                        audio.clock_rate,
                        profile.dtmf.as_ref().map(|dtmf| dtmf.payload_type),
                    )
                })
            });
            let next = (
                audio.codec,
                audio.payload_type,
                audio.clock_rate,
                dtmf_payload_type,
            );
            if previous != Some(next) {
                self.egress
                    .update_codec(EgressCodec {
                        codec: audio.codec,
                        payload_type: audio.payload_type,
                        clock_rate: audio.clock_rate,
                        dtmf_payload_type,
                        comfort_noise: self.comfort_noise,
                        comfort_noise_level_db: self.comfort_noise_level_db,
                    })
                    .await?;
            }
            sync_sender_codec(
                &self.pc,
                audio.payload_type,
                audio.clock_rate,
                audio.channels,
            );
        }
        self.apply_profile(&profile);

        let remote_dtmf_pts: Vec<u8> = negotiate::MediaNegotiator::extract_dtmf_codecs(remote)
            .iter()
            .map(|c| c.payload_type)
            .collect();
        if !remote_dtmf_pts.is_empty() {
            let mut all_pts: Vec<u8> = profile.dtmf_pts().into_iter().collect();
            all_pts.extend(remote_dtmf_pts);
            all_pts.sort();
            all_pts.dedup();
            self.tap.set_dtmf_payload_types(all_pts);
        }

        Ok(local_sdp)
    }

    /// Convenience: apply a remote offer and produce an answer.
    pub async fn answer(&self, remote_offer: &str) -> Result<String> {
        self.apply_sdp(remote_offer, SdpType::Offer).await
    }

    /// re-INVITE: apply a new remote offer, return the answer. Performs the
    /// DTLS check + R1 sender sync; the PC and egress pipeline stay alive.
    pub async fn reinvite(&self, remote_offer: &str) -> Result<String> {
        self.answer(remote_offer).await
    }

    fn apply_profile(&self, profile: &NegotiatedLegProfile) {
        *self.negotiated.lock() = Some(profile.clone());
        // DTMF telephone-event payload types → tap. Listen on ALL negotiated
        // telephone-event PTs (a WebRTC peer may send DTMF on any of them, e.g.
        // the 8 kHz PT for browsers), not just the single preferred `dtmf`.
        let dtmf_pts: Vec<u8> = profile.dtmf_pts().into_iter().collect();
        self.tap.set_dtmf_payload_types(dtmf_pts);
        // Sync the outbound DTMF send state so RFC 2833 packets use the
        // negotiated telephone-event payload type and clock rate.
        let mut dtmf = self.dtmf_send.lock();
        dtmf.dtmf_pt = profile.dtmf.as_ref().map(|d| d.payload_type);
        dtmf.dtmf_clock_rate = profile.dtmf.as_ref().map(|d| d.clock_rate).unwrap_or(8000);
    }

    /// Update the leg's negotiated profile from an SDP. Used by the session's
    /// re-INVITE path, which builds answers outside [`Self::apply_sdp`]; keeps
    /// `MediaBridge::bridge()` re-evaluation in sync with the renegotiated
    /// codec instead of the stale call-setup profile.
    pub fn apply_profile_from_sdp(&self, sdp: &str) {
        let profile = negotiate::MediaNegotiator::extract_leg_profile(sdp);
        self.apply_profile(&profile);
    }

    // ── Egress control ───────────────────────────────────────────────────

    /// Whether the leg's egress source is the fast-path relay (vs the paced
    /// egress pipeline: silence / media / transcode).
    pub fn egress_is_relay(&self) -> bool {
        self.was_relay.load(Ordering::SeqCst)
    }

    /// Switch the egress source. [`EgressSource::RewriteRelay`] is routed to
    /// the PC's rewrite bridge (transport-level, ICE exclusively owned); all
    /// other sources go to the always-alive [`EgressPipeline`].
    pub async fn set_egress_source(&self, source: EgressSource) -> Result<()> {
        let is_relay = matches!(&source, EgressSource::RewriteRelay { .. });
        let prev_was_relay = self.was_relay.swap(is_relay, Ordering::SeqCst);

        match &source {
            // Switching TO RewriteRelay: (re)arm the rewrite bridge on this PC.
            EgressSource::RewriteRelay {
                peer_pc,
                options,
                rules,
            } => {
                // The rewrite bridge needs both RTP transports ready (they are
                // created during SDP negotiation / DTLS start). Block until
                // they exist instead of proceeding and failing
                // bridge_rtp_with_rewrite_rules, which previously left the relay
                // un-armed with only a WARN in MediaBridge::accept.
                //
                // Exception: a WebRTC peer's SRTP transport only exists after
                // the remote has received our answer (200 OK) and completed
                // DTLS. Waiting on it here synchronously deadlocks call setup
                // (the 200 OK is sent only after this function returns), so
                // defer the arming to a background task and return
                // immediately. RTP-mode transports are created during SDP
                // application and are ready synchronously.
                let timeout = std::time::Duration::from_secs(2);
                let has_webrtc_peer = self.pc.config().transport_mode == TransportMode::WebRtc
                    || peer_pc.config().transport_mode == TransportMode::WebRtc;
                if has_webrtc_peer {
                    let pc = self.pc.clone();
                    let peer = peer_pc.clone();
                    let options = options.clone();
                    let rules = rules.clone();
                    let handle = tokio::spawn(async move {
                        let result =
                            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                pc.wait_for_rtp_transport_ready(timeout).await?;
                                peer.wait_for_rtp_transport_ready(timeout).await?;
                                pc.clear_rtp_rewrite_bridge();
                                pc.bridge_rtp_with_rewrite_rules(&peer, options, &rules)?;
                                Ok::<_, anyhow::Error>(())
                            })
                            .await;
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                debug!(error = %e, "deferred fast-path relay arming failed");
                            }
                            Err(_) => {
                                debug!("deferred fast-path relay arming timed out");
                            }
                        }
                    });
                    // Replace any previous arming task: a stale task must not
                    // arm a rewrite bridge on a PC we've since re-purposed.
                    if let Some(prev) = self.relay_arm_task.lock().replace(handle) {
                        prev.abort();
                    }
                } else {
                    self.pc.wait_for_rtp_transport_ready(timeout).await?;
                    peer_pc.wait_for_rtp_transport_ready(timeout).await?;
                    self.pc.clear_rtp_rewrite_bridge();
                    self.pc
                        .bridge_rtp_with_rewrite_rules(peer_pc, options.clone(), rules)?;
                }
            }
            // Switching FROM RewriteRelay: tear the rewrite bridge down so the
            // sender owns the ICE send channel again.
            _ if prev_was_relay => {
                self.pc.clear_rtp_rewrite_bridge();
            }
            _ => {}
        }

        // Forward to the pipeline. For RewriteRelay the pacing loop parks
        // (tick skipped); for everything else it produces frames.
        self.egress.set_source(source).await
    }

    /// Play a media source (IVR greeting / hold music / announcement).
    /// `on_end` fires when playback stops: `false` on natural EOF, `true` when
    /// interrupted (source switched away or leg stopped).
    pub async fn play(
        &self,
        audio: Box<dyn crate::audio_source::AudioSource>,
        loop_playback: bool,
        on_end: Option<crate::egress::EgressEndCallback>,
    ) -> Result<()> {
        self.set_egress_source(EgressSource::Media {
            audio,
            loop_playback,
            on_end,
        })
        .await
    }

    /// Put the leg on hold: play hold music (looping) or silence.
    /// Send RTP DTMF digits (RFC 2833 / RFC 4733 telephone-event) to the leg's
    /// remote peer via the egress sender.
    ///
    /// Each digit is sent as a start packet (E bit clear, duration 0) followed
    /// by an end packet (E bit set, duration 160 = 20 ms @ 8 kHz). The packets
    /// are stamped with the negotiated telephone-event payload type (not the
    /// audio codec PT) and use the leg's audio sender SSRC so the remote maps
    /// them onto the correct stream.
    ///
    /// The send state (sequence / timestamp) is per-leg and advances per digit,
    /// so consecutive `send_dtmf` calls produce coherent RTP. Digits that are
    /// not valid DTMF characters are skipped. A no-op (Ok) before the leg has
    /// negotiated a telephone-event codec.
    pub async fn send_dtmf(&self, digits: &str) -> Result<()> {
        let ssrc = sender_ssrc(&self.pc);
        if ssrc == 0 {
            return Err(anyhow!("leg {} has no audio sender SSRC yet", self.id));
        }
        let Some(dtmf_pt) = self.dtmf_send.lock().dtmf_pt else {
            return Ok(()); // no negotiated telephone-event codec
        };

        let mut digits_sent = 0usize;
        for c in digits.chars() {
            let Some(code) = crate::telephone_event::dtmf_char_to_code(c) else {
                continue;
            };
            let (seq, ts, duration) = {
                let mut st = self.dtmf_send.lock();
                let (seq, ts) = (st.sequence, st.timestamp);
                st.sequence = st.sequence.wrapping_add(2);
                let duration = dtmf_event_duration_for_clock(st.dtmf_clock_rate);
                st.timestamp = st.timestamp.wrapping_add(duration as u32);
                (seq, ts, duration)
            };

            // Start packet: E=0, duration=0.
            let start = RtpPacket::new(
                RtpHeader::new(dtmf_pt, seq, ts, ssrc),
                crate::telephone_event::telephone_event_payload(code, false, 0),
            );
            self.pc.send_raw_rtp(start).await?;
            // End packet: E=1, duration = total event length (20 ms in the
            // negotiated telephone-event clock units).
            let end = RtpPacket::new(
                RtpHeader::new(dtmf_pt, seq.wrapping_add(1), ts, ssrc),
                crate::telephone_event::telephone_event_payload(code, true, duration),
            );
            self.pc.send_raw_rtp(end).await?;
            digits_sent += 1;
        }
        debug!(leg = %self.id, digits_sent, "send_dtmf: RFC 2833 telephone-events sent");
        Ok(())
    }

    // ── RTP inactivity timeout ────────────────────────────────────────────

    /// The shared timeout state, read by the per-leg DTMF forward task which
    /// monitors it (no dedicated spawn).
    pub fn rtp_timeout_state(&self) -> Arc<RtpTimeoutState> {
        self.rtp_timeout.clone()
    }

    /// Arm the RTP inactivity timeout. Returns a `oneshot::Receiver` that fires
    /// (`Ok(())`) when no ingress packet arrives within `duration`. Disarming
    /// drops the sender → the receiver gets `Err(Canceled)`.
    ///
    /// The caller should re-arm (with a fresh receiver) on resume to restart
    /// the countdown. Idempotent.
    pub fn arm_rtp_timeout(&self, duration: Duration) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let state = &self.rtp_timeout;
        state.active.store(true, Ordering::Release);
        state
            .duration_ms
            .store(duration.as_millis() as u64, Ordering::Relaxed);
        *state.armed_at.lock() = Some(Instant::now());
        *state.fire_tx.lock() = Some(tx);
        state.armed.notify_one();
        rx
    }

    /// Pause the timeout (e.g. during hold). The monitor task keeps ticking but
    /// does not fire while paused.
    pub fn pause_rtp_timeout(&self) {
        self.rtp_timeout.active.store(false, Ordering::Release);
    }

    /// Resume the timeout. Restarts the countdown from now.
    pub fn resume_rtp_timeout(&self) {
        let state = &self.rtp_timeout;
        state.active.store(true, Ordering::Release);
        *state.armed_at.lock() = Some(Instant::now());
        state.armed.notify_one();
    }

    /// Set the app-level suppression flag. When `true` the monitor never fires
    /// regardless of `active`; used while an app drives the session or during a
    /// blind transfer (both periods where a leg may legitimately stay silent).
    pub fn set_app_paused(&self, paused: bool) {
        self.rtp_timeout.app_paused.store(paused, Ordering::Release);
    }

    /// Disarm the timeout. Drops the fire sender → a pending receiver gets
    /// `Err(Canceled)`.
    pub fn disarm_rtp_timeout(&self) {
        let state = &self.rtp_timeout;
        state.active.store(false, Ordering::Release);
        *state.armed_at.lock() = None;
        *state.fire_tx.lock() = None;
    }

    /// Fire the timeout if armed (called by the monitor task when no packets
    /// have arrived within the duration). Consumes the sender.
    pub(crate) fn fire_rtp_timeout(&self) {
        let state = &self.rtp_timeout;
        state.active.store(false, Ordering::Release);
        if let Some(tx) = state.fire_tx.lock().take() {
            debug!(leg = %self.id, "RTP inactivity timeout fired (no ingress packets)");
            let _ = tx.send(());
        }
    }

    /// Stop the leg: cancel the egress pipeline and close the PeerConnection.
    pub fn stop(&self) {
        self.egress.stop();
        if let Some(handle) = self.observer_task.lock().take() {
            handle.abort();
        }
        if let Some(handle) = self.relay_arm_task.lock().take() {
            handle.abort();
        }
        // Break the cross-leg RTP rewrite-bridge cycle BEFORE closing: in the
        // relay fastpath, leg A's transport holds a RewriteBridge whose
        // `target` is leg B's RtpTransport (and vice versa). rustrtc's
        // `PeerConnection::close` does not clear the bridge, so without this
        // the two `Arc<RtpTransport>`s keep each other alive forever and both
        // PeerConnections leak (~16KB per call in the mediaproxy=all path).
        self.pc.clear_rtp_rewrite_bridge();
        self.tap.finalize_recorder();
        self.pc.close();
    }
}

impl Drop for LegInner {
    fn drop(&mut self) {
        // Egress cancellation + PC close are fully synchronous (rustrtc close
        // path has no tokio::spawn), so this never panics during teardown.
        self.egress.stop();
        if let Some(handle) = self.observer_task.get_mut().take() {
            handle.abort();
        }
        if let Some(handle) = self.relay_arm_task.get_mut().take() {
            handle.abort();
        }
        // Break the cross-leg rewrite-bridge Arc cycle before close (see
        // `LegInner::stop`); otherwise the two RtpTransports keep each other
        // alive and the PeerConnections leak.
        self.pc.clear_rtp_rewrite_bridge();
        self.pc.close();
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn build_rtc_config(cfg: &LegConfig) -> RtcConfiguration {
    RtcConfiguration {
        transport_mode: cfg.transport.clone(),
        buffer_drop_strategy: BufferDropStrategy::DropOldest,
        // ICE pre-ready buffering: packets are buffered only until the RTP
        // transport is set up, and DropOldest means depth stays tiny in steady
        // state. 500 reserved ~5x the rustrtc default (100) and is almost never
        // reached, so restore the default to cut per-leg reserved memory.
        rtp_buffer_capacity: 100,
        runtime_handle: tokio::runtime::Handle::try_current().ok(),
        media_capabilities: Some(rustrtc::config::MediaCapabilities {
            audio: cfg.codecs.iter().map(audio_capability_from_codec).collect(),
            video: cfg.video_codecs.clone(),
            application: Some(rustrtc::config::ApplicationCapability::default()),
            image: vec![],
        }),
        ..Default::default()
    }
}

fn audio_capability_from_codec(c: &CodecInfo) -> rustrtc::config::AudioCapability {
    rustrtc::config::AudioCapability {
        payload_type: c.payload_type,
        codec_name: c.codec_name().to_string(),
        clock_rate: c.clock_rate,
        channels: c.channels as u8,
        fmtp: c.fmtp.clone(),
        rtcp_fbs: vec![],
    }
}

pub(crate) fn set_local(pc: &PeerConnection, desc: SessionDescription) -> Result<String> {
    pc.set_local_description(desc)?;
    let desc = pc
        .local_description()
        .ok_or_else(|| anyhow!("missing local description after set_local"))?;
    Ok(desc.to_sdp_string())
}

/// R1: update the sender's params so it stamps the negotiated PT. rustrtc's
/// `handle_reinvite` (>= 0.3.111) does this internally, but for an answer that
/// changes the send codec we re-assert it to be safe across versions.
fn sync_sender_codec(pc: &PeerConnection, payload_type: u8, clock_rate: u32, channels: u16) {
    for t in pc.get_transceivers() {
        if let Some(sender) = t.sender() {
            sender.set_params(RtpCodecParameters {
                payload_type,
                clock_rate,
                channels: channels as u8,
            });
        }
    }
}

/// The sender SSRC of a PC for the given media kind — the SSRC the remote peer
/// expects in RTP packets from this leg (advertised in the local SDP
/// `a=ssrc` attribute).
pub fn sender_ssrc_for_kind(pc: &PeerConnection, kind: rustrtc::MediaKind) -> u32 {
    pc.get_transceivers()
        .into_iter()
        .find(|t| t.kind() == kind)
        .and_then(|t| t.sender())
        .map(|s| s.ssrc())
        .unwrap_or(0)
}

/// The audio sender SSRC of a PC.
fn sender_ssrc(pc: &PeerConnection) -> u32 {
    sender_ssrc_for_kind(pc, rustrtc::MediaKind::Audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_transport_picks_rtp_vs_webrtc() {
        assert_eq!(
            negotiate::detect_transport("v=0\r\nm=audio 1234 RTP/AVP 0\r\n"),
            TransportMode::Rtp
        );
        assert_eq!(
            negotiate::detect_transport(
                "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=fingerprint:sha-256 XX\r\n"
            ),
            TransportMode::WebRtc
        );
    }

    #[test]
    fn dtmf_event_duration_scales_with_telephone_event_clock() {
        // RFC 4733: the duration field is in the negotiated telephone-event
        // clock's units. A 20 ms event is 160 @ 8 kHz, 320 @ 16 kHz, 960 @ 48 kHz.
        assert_eq!(dtmf_event_duration_for_clock(8000), 160);
        assert_eq!(dtmf_event_duration_for_clock(16000), 320);
        assert_eq!(dtmf_event_duration_for_clock(48000), 960);
    }

    /// When a leg negotiates multiple telephone-event PTs (e.g. WebRTC answer
    /// with both 110 telephone-event/48000 and 126 telephone-event/8000), the
    /// ingress tap must detect DTMF on ANY of them. Browsers send DTMF on the
    /// 8 kHz PT (126); if the tap only listened on the preferred 48 kHz PT
    /// (110) the digit would be silently dropped.
    #[tokio::test]
    async fn leg_tap_detects_dtmf_on_any_negotiated_telephone_event_pt() {
        use rustrtc::peer_connection::RtpObserver;
        use rustrtc::rtp::{RtpHeader, RtpPacket};
        use std::net::SocketAddr;

        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 0.0.0.0\r\n\
            t=0 0\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111 110 126\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";
        let profile = negotiate::MediaNegotiator::extract_leg_profile(sdp);
        assert!(profile.dtmf_pts().contains(&110));
        assert!(profile.dtmf_pts().contains(&126));

        let leg = LegInner::new("dtmf-leg", &LegConfig::rtp_pcmu()).expect("leg");
        leg.apply_profile(&profile);

        let tap = leg.ingress_tap();
        let mut rx = tap.subscribe_dtmf();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

        // DTMF "1" = code 0x01, sent on PT 126 (the 8 kHz browser PT).
        let pkt = RtpPacket::new(RtpHeader::new(126, 1, 0, 1234), vec![1u8, 0x80, 10, 0xA0]);
        tap.on_ingress(&pkt, addr);
        let ev = rx
            .try_recv()
            .expect("DTMF on negotiated 8 kHz PT (126) must be detected");
        assert_eq!(ev.digit, '1');

        leg.stop();
    }

    #[tokio::test]
    async fn leg_create_and_close_rtp() {
        // Two RTP legs bound to ephemeral ports must construct and stop
        // without panicking. Uses the loopback PC (no real network needed for
        // construction; add_track + observer are synchronous).
        let a = LegInner::new("a", &LegConfig::rtp_pcmu()).expect("leg a");
        assert_eq!(a.id().as_str(), "a");
        assert!(a.negotiated().is_none());
        // Observer is installed: stats start at zero.
        assert_eq!(a.stats().ingress_packets, 0);
        a.stop();
    }

    #[tokio::test]
    async fn webrtc_leg_generates_dtls_offer() {
        // A WebRTC (DTLS-SRTP) leg must produce a real WebRTC offer with a
        // DTLS fingerprint, ICE creds and a UDP/TLS/RTP/SAVPF m-line — this is
        // the proxy-side capability P6 real WebRTC e2e relies on.
        let cfg = LegConfig {
            transport: TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: CodecType::Opus,
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
        let a = LegInner::new("a", &cfg).expect("webrtc leg");
        let offer = a.create_offer(vec![]).await.expect("create_offer");
        assert!(
            offer.contains("a=fingerprint"),
            "offer lacks DTLS fingerprint:\n{}",
            offer
        );
        assert!(
            offer.contains("a=ice-ufrag"),
            "offer lacks ICE credentials:\n{}",
            offer
        );
        assert!(
            offer.contains("UDP/TLS/RTP/SAVPF"),
            "offer lacks DTLS-SRTP m-line:\n{}",
            offer
        );
        assert!(
            offer.contains("rtpmap:111 opus"),
            "offer lacks opus rtpmap:\n{}",
            offer
        );
        a.stop();
    }

    #[tokio::test]
    async fn leg_with_video_caps_emits_video_mline_with_ssrc() {
        // A leg configured with H264+VP8 video capabilities must include a
        // video m-line in its offer carrying both codecs AND an `a=ssrc`
        // attribute. The a=ssrc is what lets the remote browser demux relayed
        // video immediately instead of waiting out the 2–3 s unsignaled-SSRC
        // demux timeout.
        let cfg = LegConfig {
            transport: TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            video_codecs: negotiate::MediaNegotiator::default_video_codecs(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("video-test".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };
        let leg = LegInner::new("video", &cfg).expect("video leg");
        let offer = leg.create_offer(vec![]).await.expect("create_offer");

        assert!(
            offer.contains("m=video"),
            "offer lacks a video m-line:\n{}",
            offer
        );
        assert!(
            offer.contains("rtpmap:96 H264/90000"),
            "offer lacks H264 rtpmap:\n{}",
            offer
        );
        assert!(
            offer.contains("rtpmap:98 VP8/90000"),
            "offer lacks VP8 rtpmap:\n{}",
            offer
        );
        assert!(
            offer.contains("a=ssrc:"),
            "offer lacks any a=ssrc:\n{}",
            offer
        );

        // The video sender SSRC (advertised via a=ssrc) must be readable so
        // the MediaBridge can use it as the relay destination SSRC.
        assert_ne!(
            sender_ssrc_for_kind(leg.pc(), rustrtc::MediaKind::Video),
            0,
            "video sender SSRC must be allocated"
        );
        leg.stop();
    }

    #[tokio::test]
    async fn video_answer_echoes_ssrc_when_remote_offers_video() {
        // Answerer path (caller leg): a WebRTC offer with a video m-line must
        // be answered with a sendrecv video m-line that carries the leg's video
        // sender a=ssrc — previously this leg had no video sender, so the video
        // m-line was recvonly with no SSRC and the caller suffered the demux
        // delay.
        let cfg = LegConfig {
            transport: TransportMode::WebRtc,
            codecs: vec![CodecInfo {
                payload_type: 111,
                codec: CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: None,
            }],
            video_codecs: negotiate::MediaNegotiator::default_video_codecs(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("video-answer".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };
        let leg = LegInner::new("answerer", &cfg).expect("answerer leg");

        // Minimal WebRTC offer with audio + video m-lines (what a browser
        // sends: sendrecv, video with an SSRC).
        let offer = "v=0\r\n\
            o=- 1 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 127.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 4000 UDP/TLS/RTP/SAVPF 111\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=mid:0\r\n\
            a=sendrecv\r\n\
            a=setup:actpass\r\n\
            a=ice-ufrag:uv50\r\n\
            a=ice-pwd:ib8b\r\n\
            a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n\
            m=video 4001 UDP/TLS/RTP/SAVPF 96 98\r\n\
            a=rtpmap:96 H264/90000\r\n\
            a=rtpmap:98 VP8/90000\r\n\
            a=mid:1\r\n\
            a=sendrecv\r\n\
            a=setup:actpass\r\n\
            a=ice-ufrag:uv50\r\n\
            a=ice-pwd:ib8b\r\n\
            a=fingerprint:sha-256 F3:04:99:7A:51:6A:C4:D7:30:46:B5:69:82:2A:38:D3:37:D9:66:5B:B6:2F:AD:D3:77:DA:F5:09:2C:9E:DF:8B\r\n";

        let answer = leg
            .apply_sdp(&offer, rustrtc::SdpType::Offer)
            .await
            .expect("leg answers video offer");
        assert!(
            answer.contains("m=video"),
            "answer lacks a video m-line:\n{}",
            answer
        );
        assert!(
            answer.contains("rtpmap:96 H264/90000"),
            "answer lacks H264 rtpmap:\n{}",
            answer
        );
        assert!(
            answer.contains("a=ssrc:"),
            "answer lacks any a=ssrc (video demux delay):\n{}",
            answer
        );
        leg.stop();
    }

    /// Regression: when the caller leg answers an offer whose telephone-event
    /// PTs are not in the local answer (rustrtc's answer formats come from
    /// media_capabilities.audio which excludes telephone-event), the ingress
    /// tap must still detect DTMF on the PTs the remote peer actually sends.
    /// baresip / sipbot both fall back to their offered telephone-event PT
    /// (e.g. 101) even when the answer omits it.
    #[tokio::test]
    async fn caller_leg_detects_dtmf_from_remote_offer_telephone_event_pt() {
        use rustrtc::peer_connection::RtpObserver;
        use rustrtc::rtp::{RtpHeader, RtpPacket};
        use std::net::SocketAddr;

        let cfg = LegConfig {
            transport: TransportMode::Rtp,
            codecs: vec![
                CodecInfo {
                    payload_type: 9,
                    codec: CodecType::G722,
                    clock_rate: 8000,
                    channels: 1,
                    fmtp: None,
                },
                CodecInfo {
                    payload_type: 0,
                    codec: CodecType::PCMU,
                    clock_rate: 8000,
                    channels: 1,
                    fmtp: None,
                },
            ],
            video_codecs: Vec::new(),
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: Some("dtmf-remote-pt".to_string()),
            comfort_noise: true,
            comfort_noise_level_db: -35.0,
        };

        let leg = LegInner::new("caller-dtmf", &cfg).expect("leg");

        let offer = "v=0\r\n\
            o=- 1 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 8000 RTP/AVP 9 0 101\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n";

        let _answer = leg
            .apply_sdp(offer, SdpType::Offer)
            .await
            .expect("leg answers baresip/sipbot offer");

        let tap = leg.ingress_tap();
        let mut rx = tap.subscribe_dtmf();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

        // DTMF "5" on PT 101 — the PT offered by a baresip / sipbot caller.
        let pkt = RtpPacket::new(RtpHeader::new(101, 1, 0, 1234), vec![5u8, 0x80, 10, 0xA0]);
        tap.on_ingress(&pkt, addr);
        let ev = rx
            .try_recv()
            .expect("DTMF on offered PT 101 must be detected after apply_sdp");
        assert_eq!(ev.digit, '5');

        leg.stop();
    }
}

#[cfg(test)]
mod p24_uac_test {
    use super::*;

    #[tokio::test]
    async fn uac_leg_apply_answer_after_own_offer() {
        // A leg generates its own offer (UAC), then applies a remote answer with
        // a codec it offered. This mirrors RWI originate's callee leg.
        let cfg = LegConfig::rtp_pcmu();
        let leg = LegInner::new("uac", &cfg).unwrap();
        let offer = leg.create_offer(vec![]).await.expect("offer");
        assert!(!offer.is_empty());

        // Build a PCMU answer like the remote peer would produce.
        let answer = format!(
            "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
        );
        let local = leg
            .apply_sdp(&answer, SdpType::Answer)
            .await
            .expect("apply answer");
        assert!(local.is_empty());
        let p = leg.negotiated().expect("negotiated profile");
        assert!(
            p.audio.is_some(),
            "audio profile must be set after apply answer"
        );
        assert_eq!(
            p.audio.as_ref().map(|a| a.codec),
            Some(audio_codec::CodecType::PCMU)
        );
        // The outbound DTMF payload type must be synced from the negotiated
        // profile (RFC 4733 telephone-event on PT 101).
        let send_state = leg.dtmf_send.lock();
        assert_eq!(send_state.dtmf_pt, Some(101));
        assert_eq!(send_state.sequence, 0);
        assert_eq!(send_state.timestamp, 0);
    }
}
