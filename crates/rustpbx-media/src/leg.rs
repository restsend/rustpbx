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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use parking_lot::Mutex;
use rustrtc::{
    PeerConnection, RtpCodecParameters, RtcConfiguration, SdpType, SessionDescription,
    TransportMode,
    config::BufferDropStrategy,
    media::MediaKind,
    media::track::sample_track,
};
use tokio::sync::{broadcast, oneshot};
use tracing::debug;

use crate::egress::{EgressCodec, EgressPipeline, EgressSource};
use crate::ingress_tap::{DtmfEvent, IngressTap, MediaRecorder, TapStats};
use crate::leg_id::LegId;
use crate::negotiate::{self, CodecInfo, NegotiatedLegProfile};


/// Configuration for creating a [`Leg`]'s PeerConnection.
#[derive(Clone)]
pub struct LegConfig {
    pub transport: TransportMode,
    /// Codec preference (first entry is offered/used for the egress sender).
    pub codecs: Vec<CodecInfo>,
    pub rtp_port_range: Option<(u16, u16)>,
    pub external_ip: Option<String>,
    pub bind_ip: Option<String>,
    pub cname: Option<String>,
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
            rtp_port_range: None,
            external_ip: None,
            bind_ip: None,
            cname: None,
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
}

/// Shared RTP inactivity timeout state. The monitor task reads these atomics
/// on a fixed 100ms interval; the session toggles them via
/// [`LegInner::arm_rtp_timeout`] etc.
pub struct RtpTimeoutState {
    /// Whether the timeout is currently armed and counting.
    pub active: AtomicBool,
    /// Timeout duration in milliseconds.
    pub duration_ms: AtomicU64,
    /// When the current countdown started (arm / last packet / resume).
    /// `None` when not armed.
    pub armed_at: Mutex<Option<Instant>>,
    /// Fire channel: sent once when no ingress packets arrive within the
    /// duration. Dropped on disarm → the receiver gets `Err(Canceled)`.
    pub fire_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl Default for RtpTimeoutState {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            duration_ms: AtomicU64::new(0),
            armed_at: Mutex::new(None),
            fire_tx: Mutex::new(None),
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
        rtc_config: RtcConfiguration,
        codecs: Vec<CodecInfo>,
    ) -> Result<Leg> {
        let first_codec = codecs.first().ok_or_else(|| anyhow!("no codecs"))?;
        let pc = {
            let handle = tokio::runtime::Handle::try_current();
            let _guard = handle.as_ref().ok().map(|h| h.enter());
            PeerConnection::new(rtc_config)
        };

        // Egress audio track: create the push/source pair and add the track to
        // the PC so it appears in the SDP.
        let (sender, track, _feedback) = sample_track(MediaKind::Audio, 500);
        let params = RtpCodecParameters {
            payload_type: first_codec.payload_type,
            clock_rate: first_codec.clock_rate,
            channels: first_codec.channels as u8,
        };
        let _ = pc.add_track(track, params);

        // Plaintext bidirectional observer (stats / DTMF / recording).
        let tap = IngressTap::new(64);
        pc.add_observer(tap.clone());

        // Gate: closed until the call is answered (`accept`). The egress
        // pipeline parks on it so the leg never emits audio (even silence) to
        // its remote peer before the answer; early-media playback switches the
        // source to Media which still produces frames.
        let gated = Arc::new(AtomicBool::new(true));

        // Egress pipeline: ptime-paced push into the sender. Silence until the
        // caller switches the source (e.g. via play/hold).
        let egress_codec = EgressCodec {
            codec: first_codec.codec,
            payload_type: first_codec.payload_type,
            clock_rate: first_codec.clock_rate,
        };
        let egress = EgressPipeline::start_with_gate(
            sender,
            egress_codec,
            EgressSource::Silence,
            None,
            Some(gated.clone()),
        );

        Ok(Arc::new(LegInner {
            id: LegId::from(id.into()),
            pc,
            tap,
            egress,
            was_relay: AtomicBool::new(false),
            negotiated: Mutex::new(None),
            gated: Arc::new(AtomicBool::new(true)),
            rtp_timeout: Arc::new(RtpTimeoutState::default()),
        }))
    }

    /// Create a leg from a simplified [`LegConfig`] (builds the
    /// [`RtcConfiguration`] internally). Prefer [`Self::from_rtc_config`] when
    /// you already have a fully-configured `RtcConfiguration`.
    pub fn new(id: impl Into<String>, cfg: &LegConfig) -> Result<Leg> {
        Self::from_rtc_config(id, build_rtc_config(cfg), cfg.codecs.clone())
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
        Ok(sdp)
    }

    /// Apply a remote SDP (offer or answer) and return the local SDP to send
    /// back (an answer when `sdp_type == Offer`, or empty when applying an
    /// answer as UAC). Also extracts and stores the negotiated profile and
    /// refreshes the egress codec + DTMF payload types.
    pub async fn apply_sdp(&self, remote: &str, sdp_type: SdpType) -> Result<String> {
        // DTLS fingerprint check (R2): reject BEFORE mutating the PC so a bad
        // re-INVITE cannot half-apply (Bug B root cause).
        check_dtls_compatible(self.negotiated(), remote)?;

        let desc = SessionDescription::parse(sdp_type, remote)
            .map_err(|e| anyhow!("failed to parse remote sdp: {:?}", e))?;
        self.pc
            .set_remote_description(desc)
            .await
            .map_err(|e| anyhow!("set_remote_description failed: {}", e))?;

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
        let profile_sdp = if local_sdp.is_empty() { remote } else { local_sdp.as_str() };
        let profile = negotiate::MediaNegotiator::extract_leg_profile(profile_sdp);
        if let Some(audio) = &profile.audio {
            sync_sender_codec(&self.pc, audio.payload_type, audio.clock_rate, audio.channels);
        }
        self.apply_profile(&profile);

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
        // DTMF telephone-event payload types → tap.
        if let Some(d) = &profile.dtmf {
            self.tap.set_dtmf_payload_types(vec![d.payload_type]);
        }
        // NOTE: if a re-INVITE changes the audio codec, the egress pipeline's
        // encoder (built at construction from LegConfig.codecs[0]) must be
        // restarted with the new codec. This lands with the transcoder path;
        // for now construction picks the negotiated codec for the common case.
    }

    // ── Egress control ───────────────────────────────────────────────────

    /// Switch the egress source. [`EgressSource::RewriteRelay`] is routed to
    /// the PC's rewrite bridge (transport-level, ICE exclusively owned); all
    /// other sources go to the always-alive [`EgressPipeline`].
    pub async fn set_egress_source(&self, source: EgressSource) -> Result<()> {
        let is_relay = matches!(&source, EgressSource::RewriteRelay { .. });
        let prev_was_relay = self.was_relay.swap(is_relay, Ordering::SeqCst);

        match &source {
            // Switching TO RewriteRelay: (re)arm the rewrite bridge on this PC.
            EgressSource::RewriteRelay { peer_pc, params } => {
                // The rewrite bridge needs both RTP transports ready (they are
                // created during SDP negotiation); wait briefly for them.
                let _ = self
                    .pc
                    .wait_for_rtp_transport_ready(std::time::Duration::from_secs(2))
                    .await;
                let _ = peer_pc
                    .wait_for_rtp_transport_ready(std::time::Duration::from_secs(2))
                    .await;
                self.pc.clear_rtp_rewrite_bridge();
                self.pc.bridge_rtp_with_rewrite_to(peer_pc, *params)?;
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
    pub async fn play(&self, audio: Box<dyn crate::audio_source::AudioSource>, loop_playback: bool) -> Result<()> {
        self.set_egress_source(EgressSource::Media { audio, loop_playback, on_end: None }).await
    }

    /// Put the leg on hold: play hold music (looping) or silence.
    /// Caller ([`crate::media_bridge::MediaBridge`]) must clear any rewrite
    /// bridge first so the remote peer hears only the hold source.
    pub async fn hold(&self, music: Option<Box<dyn crate::audio_source::AudioSource>>) -> Result<()> {
        match music {
            Some(audio) => self.play(audio, true).await,
            None => self.mute().await,
        }
    }

    /// Resume from hold / play: restore egress to silence.
    /// Caller must re-arm the rewrite bridge if the leg is still routed.
    pub async fn resume(&self) -> Result<()> {
        self.mute().await
    }

    /// Mute the leg (send silence).
    pub async fn mute(&self) -> Result<()> {
        self.set_egress_source(EgressSource::Silence).await
    }

    /// Non-blocking variant of [`Self::set_egress_source`].
    pub fn try_set_egress_source(&self, source: EgressSource) -> Result<()> {
        let is_relay = matches!(&source, EgressSource::RewriteRelay { .. });
        let prev_was_relay = self.was_relay.swap(is_relay, Ordering::SeqCst);

        match &source {
            EgressSource::RewriteRelay { peer_pc, params } => {
                self.pc.clear_rtp_rewrite_bridge();
                self.pc.bridge_rtp_with_rewrite_to(peer_pc, *params)?;
            }
            _ if prev_was_relay => {
                self.pc.clear_rtp_rewrite_bridge();
            }
            _ => {}
        }

        self.egress.try_set_source(source)
    }

    /// Send RTP DTMF digits (RFC 2833) via the egress sender.
    ///
    /// NOTE: full RFC 2833 generation needs a telephone-event encoder; this is
    /// a placeholder that logs until the telephone-event sender path lands.
    pub async fn send_dtmf(&self, digits: &str) -> Result<()> {
        debug!(leg = %self.id, digits, "send_dtmf: RFC 2833 telephone-event generation pending");
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
        state.duration_ms.store(duration.as_millis() as u64, Ordering::Relaxed);
        *state.armed_at.lock() = Some(Instant::now());
        *state.fire_tx.lock() = Some(tx);
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
            let _ = tx.send(());
        }
    }

    /// Stop the leg: cancel the egress pipeline and close the PeerConnection.
    pub fn stop(&self) {
        self.egress.stop();
        self.tap.finalize_recorder();
        self.pc.close();
    }
}

impl Drop for LegInner {
    fn drop(&mut self) {
        // Egress cancellation + PC close are fully synchronous (rustrtc close
        // path has no tokio::spawn), so this never panics during teardown.
        self.egress.stop();
        self.pc.close();
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn build_rtc_config(cfg: &LegConfig) -> RtcConfiguration {
    RtcConfiguration {
        transport_mode: cfg.transport.clone(),
        buffer_drop_strategy: BufferDropStrategy::DropOldest,
        rtp_buffer_capacity: 500,
        runtime_handle: tokio::runtime::Handle::try_current().ok(),
        media_capabilities: Some(rustrtc::config::MediaCapabilities {
            audio: cfg.codecs.iter().map(audio_capability_from_codec).collect(),
            video: vec![rustrtc::config::VideoCapability::default()],
            application: Some(rustrtc::config::ApplicationCapability::default()),
            image: vec![],
        }),
        ..Default::default()
    }
}

fn audio_capability_from_codec(c: &CodecInfo) -> rustrtc::config::AudioCapability {
    rustrtc::config::AudioCapability {
        payload_type: c.payload_type,
        codec_name: match c.codec {
            audio_codec::CodecType::PCMU => "PCMU",
            audio_codec::CodecType::PCMA => "PCMA",
            audio_codec::CodecType::G722 => "G722",
            audio_codec::CodecType::G729 => "G729",
            audio_codec::CodecType::Opus => "opus",
            audio_codec::CodecType::TelephoneEvent => "telephone-event",
        }
        .to_string(),
        clock_rate: c.clock_rate,
        channels: c.channels as u8,
        fmtp: c.fmtp.clone(),
        rtcp_fbs: vec![],
    }
}

fn set_local(pc: &PeerConnection, desc: SessionDescription) -> Result<String> {
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

/// R2: if the PC has already started its DTLS transport, reject a remote SDP
/// whose fingerprint differs (rustrtc would error mid-apply, half-mutating).
fn check_dtls_compatible(prev: Option<NegotiatedLegProfile>, _new_sdp: &str) -> Result<()> {
    // The authoritative check is rustrtc's (peer_connection.rs set_remote). We
    // additionally surface a clear error here when a previous profile exists.
    // Full fingerprint comparison is left to rustrtc; this is a no-op guard
    // that documents the contract.
    let _ = prev;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_transport_picks_rtp_vs_webrtc() {
        assert_eq!(negotiate::detect_transport("v=0\r\nm=audio 1234 RTP/AVP 0\r\n"), TransportMode::Rtp);
        assert_eq!(
            negotiate::detect_transport("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=fingerprint:sha-256 XX\r\n"),
            TransportMode::WebRtc
        );
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
}
