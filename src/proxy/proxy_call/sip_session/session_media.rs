use super::prelude::*;
use super::session::SipSession;
use crate::proxy::call::parse_allowed_codecs;
use crate::proxy::proxy_call::sip_session::util::trunk_host_port;


// ===================================================================
// Media negotiation & SDP handling for SipSession
// (moved from sip_session/session.rs — the media-domain cluster:
// SDP parse/offer/answer/codecs, media legs, early media, video,
// media health monitor, media-path updates).
// ===================================================================

impl SipSession {
    /// Build a `LegConfig` for a MediaBridge leg from the dialplan media
    /// settings. Shared by all leg-creation paths (`ensure_media_leg`,
    /// `ensure_caller_leg`, `create_callee_track`) so transport/codec/port
    /// handling stays consistent.
    /// Whether the media path carries video: only `VideoPolicy::Strip`
    /// disables it (video transcoding is not implemented, so the other
    /// policies all relay at the transport level).
    pub(crate) fn video_relay_enabled(&self) -> bool {
        !matches!(
            self.context.dialplan.media.video_policy,
            Some(crate::proxy::routing::VideoPolicy::Strip)
        )
    }

    pub(crate) fn video_caps_from_sdp(&self, sdp: &str) -> Vec<rustrtc::VideoCapability> {
        let proxy_config = self.server.proxy_config.load();
        crate::media::negotiate::MediaNegotiator::video_caps_for_config(
            &crate::media::negotiate::MediaNegotiator::extract_video_codecs(sdp),
            &proxy_config.video_codecs,
        )
    }

    /// Media endpoints outlive bridge membership and are owned by the session.
    pub fn media_leg(&self, id: &LegId) -> Option<crate::media::leg::Leg> {
        self.legs.media_leg(id)
    }

    pub(crate) fn media_side_for_leg(&self, id: &LegId) -> Option<LegSide> {
        self.bridge().and_then(|mb| mb.side_for_leg(id))
    }

    pub(crate) fn check_media_proxy(context: &CallContext, mode: &MediaProxyMode) -> bool {
        if context.dialplan.recording.enabled {
            return true;
        }

        let app_or_queue_flow = matches!(
            context.dialplan.flow,
            crate::call::DialplanFlow::Application { .. } | crate::call::DialplanFlow::Queue { .. }
        );

        match mode {
            MediaProxyMode::All => true,
            // In Auto/NAT mode, keep media anchored for app/queue flows because
            // they require playback/injection capabilities.
            MediaProxyMode::Auto | MediaProxyMode::Nat => app_or_queue_flow,
            MediaProxyMode::None => false,
            _ => false,
        }
    }

    /// True when media bypasses the local bridge entirely (pure B2BUA
    /// passthrough at the signaling layer).
    pub(super) fn bypasses_local_media(&self) -> bool {
        self.media_profile.path == MediaPathMode::Bypass
            && self.media_leg(&LegId::from("caller")).is_none()
    }

    /// Monitor the bridge's media-health snapshots: append a bounded number
    /// of `media_health` trace entries (periodic + at first stall) and fire
    /// `proxy.media_stalled` (CDR error chip + RWI `call_error`) once per
    /// leg that keeps receiving zero inbound RTP while the route is active —
    /// the mid-call signal for firewall / mis-advertised-address black holes.
    pub(crate) fn arm_media_health_monitor(
        mb: &crate::media::media_bridge::MediaBridge,
        cmd_tx: Option<mpsc::Sender<CallCommand>>,
        session_id: &str,
        stall_detect_secs: Option<u64>,
        trace_interval_secs: Option<u64>,
    ) {
        match stall_detect_secs {
            // `0` disables stall detection: route age can never reach
            // `Duration::MAX`, so no leg is ever flagged stalled.
            Some(0) => mb.set_stall_detect(std::time::Duration::MAX),
            Some(secs) => {
                mb.set_stall_detect(std::time::Duration::from_secs(secs));
            }
            None => {} // bridge default (15s)
        }
        let mut rx = mb.health_rx();
        let sid = session_id.to_string();
        let reported: Arc<parking_lot::Mutex<std::collections::HashSet<String>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
        // Periodic snapshots: 5s sampler tick, publish every
        // `trace_interval_secs`, bounded to 20 entries per call so long
        // calls cannot bloat the CDR metadata.
        let every_ticks = match trace_interval_secs.unwrap_or(30) {
            0 => 0,
            secs => (secs as usize / 5).max(1),
        };
        let tick: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let periodic_emitted: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        const MAX_PERIODIC_TRACE_EVENTS: usize = 20;
        crate::utils::spawn(async move {
            loop {
                match rx.changed().await {
                    Ok(()) => {}
                    Err(_) => return, // bridge dropped — session is over
                }
                let Some(snapshot) = rx.borrow_and_update().clone() else {
                    continue;
                };
                let n = tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if every_ticks > 0
                    && n % every_ticks == 0
                    && periodic_emitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        < MAX_PERIODIC_TRACE_EVENTS
                {
                    if let Ok(detail) = serde_json::to_value(snapshot.as_ref()) {
                        if let Some(tx) = &cmd_tx {
                            let _ = tx.try_send(CallCommand::Trace {
                                event: crate::call_errors::TraceEvent::new(
                                    crate::call_errors::TraceKind::MediaHealth,
                                    format!("media health @ {:.0}s", snapshot.route_age_secs),
                                )
                                .detail(detail),
                            });
                        }
                    }
                }
                for side in snapshot.stalled_sides() {
                    if !reported.lock().insert(side.to_string()) {
                        continue; // once per leg per call
                    }
                    let detail = serde_json::to_value(snapshot.as_ref()).ok();
                    warn!(
                        session_id = %sid,
                        leg = side,
                        route_age_s = format!("{:.0}", snapshot.route_age_secs),
                        "media stalled: no inbound RTP on connected leg"
                    );
                    if let Some(tx) = &cmd_tx {
                        let _ = tx.try_send(CallCommand::ReportCallError {
                            stage: "proxy".to_string(),
                            app: "proxy".to_string(),
                            code: "proxy.media_stalled".to_string(),
                            severity: crate::call_errors::ErrSeverity::Warn,
                            message: format!("no inbound media on {side} leg"),
                            sip_status: None,
                            detail,
                        });
                    }
                }
            }
        });
    }

    /// Ensure the proxy role has an independent media peer, using the codecs
    /// negotiated in `sdp` if needed. Idempotent.
    pub(crate) async fn ensure_media_leg(
        &mut self,
        side: LegSide,
        sdp: &str,
        transport: rustrtc::TransportMode,
    ) -> Result<()> {
        let codecs = crate::media::negotiate::MediaNegotiator::extract_codec_params(sdp).audio;
        // Answerer legs need a video transceiver whenever the remote offer
        // carries video (rustrtc's answer builder requires a transceiver per
        // remote section), so the caps are always derived from `sdp`. The video
        // policy (strip) is enforced when the answer SDP is assembled.
        let video_codecs = self.video_caps_from_sdp(sdp);
        let cfg = self.build_leg_config(transport, codecs, video_codecs);
        let leg_name = match side {
            LegSide::A => "caller",
            LegSide::B => "callee",
        };
        if self.media_leg(&LegId::from(leg_name)).is_some() {
            return Ok(());
        }
        let recorder_sender = match side {
            LegSide::A => self.setup_recording_capture()?,
            LegSide::B => {
                // The callee leg feeds the SAME recording task so its ingress
                // (the agent mic) can become the recorder's B channel while
                // the call is held. The tap starts fully off -- the caller leg
                // owns both channels while bridged -- and is retargeted by
                // `apply_hold_recording_routes` on hold.
                self.media.recording.capture_sender()
            }
        };
        let leg = crate::media::leg::LegInner::new(leg_name, &cfg, recorder_sender)?;
        self.legs.set_media_leg(&LegId::from(leg_name), leg);
        if side == LegSide::B
            && let Some(leg) = self.media_leg(&LegId::from(leg_name))
        {
            // Starts fully OFF: while bridged the caller leg's egress owns the
            // B channel (relayed far-party audio). A second live source on the
            // same sequential channel would interleave into garbage. Hold
            // flips this on via `apply_hold_recording_routes`.
            leg.ingress_tap().set_capture_route(None, None);
        }
        if side == LegSide::A {
            self.spawn_dtmf_forwarder();
        }
        Ok(())
    }

    pub(crate) fn extract_sdp(body: &[u8]) -> Option<String> {
        if body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(body).to_string())
        }
    }

    pub(crate) fn sdp_headers() -> Vec<rsipstack::sip::Header> {
        vec![rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        )]
    }

    /// Negotiate a video m-line with the opposite anchored leg before
    /// answering a re-INVITE that adds video. The outgoing SDP is generated by
    /// that leg's own PeerConnection, so it advertises RustPBX's transport
    /// addresses rather than leaking the source endpoint's addresses through
    /// the B2BUA.
    pub(crate) async fn negotiate_added_video_with_peer(
        &mut self,
        source_leg_id: &LegId,
        target_leg_id: &LegId,
        method: rsipstack::sip::Method,
        offered_caps: &[rustrtc::VideoCapability],
    ) -> Result<Vec<rustrtc::VideoCapability>> {
        if offered_caps.is_empty() {
            return Ok(Vec::new());
        }
        let target_leg = self
            .media_leg(target_leg_id)
            .ok_or_else(|| anyhow!("opposite media leg is unavailable for video re-INVITE"))?;

        // Only a BUNDLE destination needs globally unique audio/video payload
        // types. Plain RTP has separate audio and video sockets, so preserving
        // an offered PT such as 96 on both m-lines is valid and avoids a
        // needless PT change during a SIP re-INVITE.
        let mut target_video_caps = offered_caps.to_vec();
        if target_leg.pc().config().transport_mode == rustrtc::TransportMode::WebRtc {
            let occupied_audio_payload_types = target_leg
                .pc()
                .config()
                .media_capabilities
                .as_ref()
                .into_iter()
                .flat_map(|capabilities| capabilities.audio.iter())
                .map(|capability| capability.payload_type);
            MediaNegotiator::remap_bundle_video_payload_types(
                &mut target_video_caps,
                occupied_audio_payload_types,
            )?;
        }
        let source_target_video_caps: Vec<_> = offered_caps
            .iter()
            .cloned()
            .zip(target_video_caps.iter().cloned())
            .collect();

        crate::media::leg::ensure_video_sender_for_pc(target_leg.pc(), &target_video_caps[0])?;
        if let Some(transceiver) = target_leg
            .pc()
            .get_transceivers()
            .into_iter()
            .find(|transceiver| transceiver.kind() == rustrtc::MediaKind::Video)
        {
            transceiver.set_direction(rustrtc::TransceiverDirection::SendRecv);
        }
        let generated_offer = target_leg.prepare_offer().await?;
        let mut target_offer_sdp = MediaNegotiator::rewrite_video_capabilities(
            rustrtc::SdpType::Offer,
            &generated_offer.to_sdp_string(),
            &target_video_caps,
        )
        .map_err(|error| anyhow!("failed to build opposite-leg video offer: {error}"))?;

        // A video-only change must not reorder or expand the already selected
        // audio codec. Keep the target leg's current audio and DTMF entries.
        if let Some(profile) = target_leg.negotiated() {
            let mut selected_audio = Vec::new();
            if let Some(audio) = profile.audio {
                selected_audio.push(audio.to_codec_info());
            }
            if let Some(dtmf) = profile.dtmf {
                selected_audio.push(dtmf.to_codec_info());
            }
            if !selected_audio.is_empty()
                && let Some(rewritten) =
                    crate::media::negotiate::MediaNegotiator::rewrite_sdp_codec_list(
                        &target_offer_sdp,
                        &selected_audio,
                    )
            {
                target_offer_sdp = rewritten;
            }
        }

        let target_offer = Self::parse_sdp(
            rustrtc::SdpType::Offer,
            &target_offer_sdp,
            "opposite-leg video re-INVITE offer",
        )?;
        let offered_codec_names: Vec<_> = offered_caps
            .iter()
            .map(|cap| cap.codec_name.as_str())
            .collect();
        info!(
            session_id = %self.id,
            %source_leg_id,
            %target_leg_id,
            codecs = ?offered_codec_names,
            "propagating added video to opposite anchored leg"
        );
        let response = self
            .send_mid_dialog_request_to_leg(
                target_leg_id,
                method,
                Self::sdp_headers(),
                Some(target_offer_sdp.as_bytes().to_vec()),
            )
            .await?
            .ok_or_else(|| anyhow!("opposite-leg video re-INVITE timed out"))?;

        if response.status_code.kind() != rsipstack::sip::status_code::StatusCodeKind::Successful {
            warn!(
                session_id = %self.id,
                %target_leg_id,
                status = %response.status_code,
                "opposite leg rejected video re-INVITE"
            );
            return Ok(Vec::new());
        }
        let peer_answer_sdp = Self::extract_sdp(response.body())
            .ok_or_else(|| anyhow!("opposite-leg video re-INVITE returned no SDP"))?;
        // The peer answers the target-leg offer and therefore echoes the
        // target-leg PT. Return the paired source capability when building the
        // source-leg answer, so a non-BUNDLE PT such as 96 remains 96 even when
        // the WebRTC target had to advertise it as 97.
        let peer_accepted_video = MediaNegotiator::extract_video_codecs(&peer_answer_sdp);
        let accepted_caps: Vec<_> = source_target_video_caps
            .iter()
            .filter(|(_, target_cap)| {
                peer_accepted_video.iter().any(|accepted_cap| {
                    accepted_cap.payload_type == target_cap.payload_type
                        && accepted_cap
                            .name
                            .eq_ignore_ascii_case(&target_cap.codec_name)
                        && accepted_cap.clock_rate == target_cap.clock_rate
                })
            })
            .map(|(source_cap, _)| source_cap.clone())
            .collect();
        let accepted_codec_names: Vec<_> = accepted_caps
            .iter()
            .map(|cap| cap.codec_name.as_str())
            .collect();
        info!(
            session_id = %self.id,
            %target_leg_id,
            codecs = ?accepted_codec_names,
            "opposite anchored leg answered added-video negotiation"
        );

        // Commit the prepared local offer only after the peer accepted it. A
        // rejection therefore leaves rustrtc in Stable instead of requiring a
        // rollback (which it does not support).
        target_leg
            .pc()
            .set_local_description(target_offer)
            .map_err(|error| anyhow!("failed to commit opposite-leg video offer: {error}"))?;
        target_leg
            .apply_sdp(&peer_answer_sdp, rustrtc::SdpType::Answer)
            .await?;
        target_leg.refresh_observer();

        self.legs
            .set_video_state(target_leg_id, !accepted_caps.is_empty());
        self.legs
            .set_answer(target_leg_id.clone(), peer_answer_sdp.clone());
        match target_leg_id.as_str() {
            "caller" => {
                self.media.caller_offer = Some(peer_answer_sdp);
                self.media.answer = Some(target_offer_sdp);
            }
            "callee" => {
                self.media.callee_offer = Some(target_offer_sdp);
                self.media.callee_answer_sdp = Some(peer_answer_sdp);
            }
            _ => {}
        }

        Ok(accepted_caps)
    }

    pub(crate) fn parse_sdp(
        sdp_type: rustrtc::SdpType,
        sdp: &str,
        context: &str,
    ) -> anyhow::Result<rustrtc::SessionDescription> {
        rustrtc::SessionDescription::parse(sdp_type, sdp)
            .map_err(|e| anyhow::anyhow!("Failed to parse {} SDP: {}", context, e))
    }

    /// Send 183 Session Progress with early media audio played to the caller.
    /// Supports file paths and `tone://frequency,duration_ms` format. The ring
    /// tone loops until the callee answers; the caller-side handle is dropped.
    pub(crate) async fn send_early_media_tone(&mut self, audio_path: &str) -> Result<()> {
        let loop_playback = !audio_path.starts_with("tone://");
        self.send_early_media(audio_path, loop_playback)
            .await
            .map(|_| ())
    }

    /// Play a one-shot early-media cue (e.g. a failure/beep tone) through the
    /// caller media peer.
    pub(crate) async fn send_early_media_cue(
        &mut self,
        audio_path: &str,
    ) -> Result<Option<crate::media::media_bridge::PlaybackHandle>> {
        let loop_playback = !audio_path.starts_with("tone://");
        self.send_early_media(audio_path, loop_playback).await
    }

    /// Prepare the caller media peer, send 183 Session Progress,
    /// and play `audio_path` as early media (ringback tone or short cue).
    /// Returns the playback handle when audio actually started.
    pub(crate) async fn send_early_media(
        &mut self,
        audio_path: &str,
        loop_playback: bool,
    ) -> Result<Option<crate::media::media_bridge::PlaybackHandle>> {
        // Ensure caller media exists before sending early audio.
        if let Err(e) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                error = %e,
                "Failed to ensure caller leg for 183 early media"
            );
            return Ok(None);
        }

        if !self.media.early_media_sent {
            // Get caller-facing answer SDP from the caller leg's PC
            let answer_sdp = self
                .media_leg(&LegId::from("caller"))
                .and_then(|leg| leg.pc().local_description())
                .map(|desc| desc.to_sdp_string())
                .or_else(|| self.media.answer.clone())
                .unwrap_or_default();

            if answer_sdp.is_empty() {
                warn!(session_id = %self.id, "Cannot send 183: no local SDP available on caller leg");
                return Ok(None);
            }

            self.media.answer = Some(answer_sdp.clone());
            self.media.early_media_sent = true;

            // Send 183 Session Progress with SDP
            let ringing_result: anyhow::Result<()> = match self.caller_dialog.as_ref() {
                Some(dialog) => dialog
                    .ringing(Some(Self::sdp_headers()), Some(answer_sdp.into_bytes()))
                    .map_err(|e| anyhow!("{}", e)),
                None => Ok(()),
            };
            if let Err(e) = ringing_result {
                warn!(session_id = %self.context.session_id, error = %e, "Failed to send 183 Session Progress");
            } else {
                info!(session_id = %self.context.session_id, "Sent 183 Session Progress with early media");
            }
        }

        let peer = self
            .media_leg(&LegId::from("caller"))
            .ok_or_else(|| anyhow!("No caller media peer"))?;
        let audio: Box<dyn crate::media::audio_source::AudioSource> =
            if let Some(spec) = audio_path.strip_prefix("tone://") {
                let (frequency, duration) = spec
                    .split_once(',')
                    .ok_or_else(|| anyhow!("Invalid tone spec: {}", spec))?;
                Box::new(crate::media::audio_source::ToneAudioSource::new(
                    frequency.trim().parse()?,
                    Duration::from_millis(
                        duration
                            .trim()
                            .parse::<u64>()?
                            .max(Self::MIN_TONE_DURATION_MS),
                    ),
                    8000,
                )?)
            } else {
                Box::new(
                    crate::media::audio_source::FileAudioSource::new(
                        Self::resolve_audio_file_path(audio_path),
                        loop_playback,
                    )
                    .await?,
                )
            };
        if let Some(mb) = self.bridge_mut() {
            mb.unbridge().await?;
        }
        let handle = peer.play_media(audio, loop_playback).await?;
        self.record_play_start("progress-media", "ringback");
        Ok(Some(handle))
    }

    /// Final audio codec normalization for answers generated by PeerConnection.
    /// This keeps answer audio as an offer subset, ordered by the peer answer
    /// when available, while preserving the caller-offered payload types.
    pub(crate) fn rewrite_answer_to_selected_audio_codecs(
        &self,
        answer_sdp: &str,
        offer_sdp: &str,
        preferred_peer_sdp: Option<&str>,
        previous_negotiated_codec: Option<CodecType>,
        context: &str,
    ) -> String {
        let allow_codecs = self.resolve_effective_codecs();
        let preferred_audio_codecs: Vec<CodecType> = preferred_peer_sdp
            .map(|sdp| {
                MediaNegotiator::extract_codec_params(sdp)
                    .audio
                    .into_iter()
                    .map(|codec| codec.codec)
                    .collect()
            })
            .filter(|codecs: &Vec<CodecType>| !codecs.is_empty())
            .unwrap_or(allow_codecs);
        let offered_audio_codecs = MediaNegotiator::extract_codec_params(offer_sdp);
        let previous_offered = previous_negotiated_codec.filter(|previous_codec| {
            offered_audio_codecs
                .audio
                .iter()
                .any(|codec| codec.codec == *previous_codec)
        });
        let selected_audio_codecs = if let Some(previous_codec) = previous_offered {
            MediaNegotiator::build_codec_list_from_offer(offer_sdp, &[previous_codec])
        } else {
            MediaNegotiator::build_codec_list_from_offer(offer_sdp, &preferred_audio_codecs)
        };
        if selected_audio_codecs.is_empty() {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                context,
                "No compatible audio codec selected for SDP answer"
            );
            return answer_sdp.to_string();
        }
        debug!(
            session_id = %self.context.session_id,
            context,
            previous_negotiated_codec = ?previous_negotiated_codec,
            selected_audio_codecs = ?selected_audio_codecs.iter().map(|c| (c.payload_type, &c.codec, c.clock_rate)).collect::<Vec<_>>(),
            "SDP answer codec selection before rewrite"
        );

        MediaNegotiator::rewrite_sdp_codec_list(answer_sdp, &selected_audio_codecs).unwrap_or_else(
            || {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    context,
                    "Failed to rewrite SDP answer to selected audio codec"
                );
                answer_sdp.to_string()
            },
        )
    }

    pub(crate) async fn prepare_callee_media_offer(
        &mut self,
        target: &crate::call::Location,
    ) -> Result<Option<Vec<u8>>> {
        let callee_is_webrtc = Self::callee_supports_webrtc(target);

        // Transport-aware parallel-fork caching. When multiple fork
        // targets share the same transport type, reuse the cached offer so all
        // forks promise the same bound port. Regenerate when transport differs
        // (e.g. one WebRTC fork and one RTP fork).
        if let Some(cached) = &self.media.callee_offer {
            if self.media.callee_offer_cached_webrtc == Some(callee_is_webrtc) {
                return Ok(Some(cached.clone().into_bytes()));
            }
        }
        self.media.callee_offer_cached_webrtc = Some(callee_is_webrtc);

        let caller_is_webrtc = self.is_caller_webrtc();
        let callee_sdp = if self.bypasses_local_media() && caller_is_webrtc == callee_is_webrtc {
            let allow_codecs = self.resolve_effective_codecs();
            if !allow_codecs.is_empty() {
                if let Some(ref caller_offer) = self.media.caller_offer {
                    let selected_codecs =
                        MediaNegotiator::build_codec_list_from_offer(caller_offer, &allow_codecs);
                    if selected_codecs.is_empty() {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            context = "bypass callee offer",
                            "No compatible audio codec selected for pass-through SDP offer"
                        );
                        Some(caller_offer.clone())
                    } else {
                        Some(MediaNegotiator::rewrite_sdp_codec_list(
                            caller_offer,
                            &selected_codecs,
                        )
                        .unwrap_or_else(|| {
                            warn!(session_id = %self.id,
                                session_id = %self.context.session_id,
                                context = "bypass callee offer",
                                "Failed to rewrite pass-through SDP offer to selected audio codec list"
                            );
                            caller_offer.clone()
                        }))
                    }
                } else {
                    self.media.caller_offer.clone()
                }
            } else {
                self.media.caller_offer.clone()
            }
        } else {
            Some(self.create_callee_track(callee_is_webrtc).await?)
        };
        self.media.callee_offer = callee_sdp.clone();
        Ok(callee_sdp.map(|s| s.into_bytes()))
    }

    /// Create an INDEPENDENT media peer for a parallel-fork leg.
    ///
    /// Unlike [`Self::prepare_callee_media_offer`], this does NOT use the
    /// shared "callee" leg or its offer cache: each fork device needs its own
    /// UDP port so the symmetric-RTP latch can bind to that specific fork's
    /// RTP source (the shared leg's latch races between forks and can stick
    /// to the loser's address, killing the winner's audio).
    pub(crate) async fn create_fork_media_offer(
        &mut self,
        target: &crate::call::Location,
        leg_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        let callee_is_webrtc = Self::callee_supports_webrtc(target);
        let mode = self.callee_transport_mode(callee_is_webrtc);
        let leg_id = crate::call::domain::LegId::from(leg_id);

        let (peer, offer) = self.create_leg_peer(&leg_id, mode).await?;
        // Ensure the logical leg entry exists before setting the media peer —
        // set_media_leg/get_mut silently drops when the entry is absent, and
        // build_target_invite_option's set_transport also no-ops on missing
        // entries, so fork-{idx} legs must be explicitly inserted here.
        if self.legs.get(&leg_id).is_none() {
            self.legs.insert(
                leg_id.clone(),
                crate::call::domain::Leg::new(leg_id.clone()),
            );
        }
        self.legs.set_media_leg(&leg_id, peer);
        Ok(Some(offer.into_bytes()))
    }

    pub(crate) async fn prepare_caller_answer_from_callee_sdp(
        &mut self,
        callee_sdp: Option<String>,
        force_regenerate: bool,
        callee_sdp_type: rustrtc::SdpType,
    ) -> Result<Option<String>> {
        let is_early_media = callee_sdp_type == rustrtc::SdpType::Pranswer;
        let rtp_timeout = self.rtp_timeout_config();

        // ── MediaBridge path: both legs exist → apply answer + bridge ────
        if let Some(callee_sdp_value) = callee_sdp.as_ref() {
            let has_callee_leg = self.media_leg(&LegId::from("callee")).is_some();

            let has_bridge =
                self.media_profile.path == MediaPathMode::Anchored || self.media.bridge.is_some();
            tracing::debug!(
                session_id = %self.id,
                has_bridge,
                has_callee_leg,
                "prepare_caller_answer_from_callee_sdp: checking MediaBridge path"
            );

            if has_bridge && has_callee_leg {
                let caller = self
                    .media_leg(&LegId::from("caller"))
                    .ok_or_else(|| anyhow!("Missing caller media peer"))?;
                let callee = self
                    .media_leg(&LegId::from("callee"))
                    .ok_or_else(|| anyhow!("Missing callee media peer"))?;
                if self.media.bridge.is_none() {
                    self.media.bridge = Some(MediaBridge::new_with_quality_stats(
                        self.id.to_string(),
                        self.context.dialplan.media.quality_stats.clone(),
                    ));
                }
                self.bridge_mut()
                    .unwrap()
                    .select_pair(caller, callee)
                    .await?;
                // Direct dialing owns this selection; later media resume restores it.
                self.bridge = BridgeConfig::bridge(LegId::from("caller"), LegId::from("callee"));
                let cmd_tx = self.cmd_tx.clone();
                let session_id = self.context.session_id.clone();
                // Once the callee answers, normalize caller audio to the
                // selected audio codec and answer caller video with only the
                // video capabilities accepted by the callee.
                let caller_answer = match (
                    self.media.answer.as_deref(),
                    self.media.caller_offer.as_deref(),
                ) {
                    (Some(answer), Some(caller_offer)) => {
                        let answer = self.rewrite_answer_to_selected_audio_codecs(
                            answer,
                            caller_offer,
                            Some(callee_sdp_value),
                            None,
                            "MediaBridge caller answer",
                        );
                        let caller_video_caps = self.video_caps_from_sdp(caller_offer);
                        let accepted_video_caps = MediaNegotiator::accepted_video_capabilities(
                            &caller_video_caps,
                            callee_sdp_value,
                        );
                        let answer = MediaNegotiator::rewrite_video_capabilities(
                            rustrtc::SdpType::Answer,
                            &answer,
                            &accepted_video_caps,
                        )
                        .map_err(|error| {
                            anyhow!("failed to build MediaBridge caller video answer: {error}")
                        })?;
                        Some(answer)
                    }
                    _ => self.media.answer.clone(),
                };
                self.media.answer = caller_answer.clone();

                {
                    let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
                    if let Some(callee_leg) = mb.leg(LegSide::B) {
                        let sdp_type = callee_sdp_type;
                        callee_leg.apply_sdp(callee_sdp_value, sdp_type).await?;
                    }

                    // Keep the actual sender, egress codec, bridge profile and
                    // recorder profile aligned with the SDP returned to caller.
                    // This must happen before accept() activates the route.
                    if let (Some(leg), Some(answer)) =
                        (mb.leg(LegSide::A), caller_answer.as_deref())
                    {
                        leg.apply_profile_from_sdp(answer).await?;
                    }
                }

                // One-shot: only the first answer path (app answer or callee
                // answer) arms the relay-arm-failure monitor — a second
                // spawn would double the warn and the fallback command.
                let arm_relay_monitor = !self.meta.relay_arm_monitor_spawned;
                if arm_relay_monitor {
                    self.meta.relay_arm_monitor_spawned = true;
                }
                // Same one-shot rule for the media-health monitor: the answer
                // path can run more than once per session, and a second
                // watcher on the same bridge would duplicate every periodic
                // media_health trace and every proxy.media_stalled error.
                let arm_health_monitor = !self.meta.media_health_monitor_spawned;
                if arm_health_monitor {
                    self.meta.media_health_monitor_spawned = true;
                }
                let stall_detect_secs = self.context.dialplan.media.stall_detect_secs;
                let trace_interval_secs = self.context.dialplan.media.media_trace_interval_secs;
                let mb = self.bridge_mut().ok_or_else(|| anyhow!("No MediaBridge"))?;
                mb.accept(LegSide::B).await;
                mb.accept(LegSide::A).await;
                if !is_early_media {
                    Self::arm_bridged_rtp_timeouts(mb, rtp_timeout, cmd_tx.clone(), &session_id);
                }
                if arm_health_monitor {
                    Self::arm_media_health_monitor(
                        mb,
                        cmd_tx.clone(),
                        &session_id,
                        stall_detect_secs,
                        trace_interval_secs,
                    );
                }
                if arm_relay_monitor {
                    Self::arm_relay_arm_failure_monitor(mb, cmd_tx, &session_id);
                }

                // A real callee answered — any in-progress transfer is over.
                self.meta.transfer_in_progress = false;
                self.sync_rtp_timeout_pause();

                self.media.callee_answer_sdp = Some(callee_sdp_value.clone());
                return Ok(caller_answer);
            }
        }

        let Some(callee_sdp_value) = callee_sdp else {
            if callee_sdp_type == rustrtc::SdpType::Answer && self.media.early_media_sent {
                debug!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    "Final 200 OK has no SDP; promoting the existing early-media descriptions to Answer"
                );

                let (caller_pc, callee_pc) = if self.media_profile.path == MediaPathMode::Anchored {
                    let caller_pc = self
                        .media_leg(&LegId::from("caller"))
                        .map(|peer| peer.pc().clone());
                    let callee_pc = self
                        .media_leg(&LegId::from("callee"))
                        .map(|peer| peer.pc().clone());
                    (caller_pc, callee_pc)
                } else {
                    (None, None)
                };

                if let Some(callee_pc) = callee_pc
                    && let Some(mut answer) = callee_pc.remote_description()
                    && answer.sdp_type == rustrtc::SdpType::Pranswer
                {
                    answer.sdp_type = rustrtc::SdpType::Answer;
                    if let Err(error) = callee_pc.set_remote_description(answer).await {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %error,
                            "Failed to promote callee remote pranswer"
                        );
                    }
                }

                if let Some(caller_pc) = caller_pc
                    && let Some(mut answer) = caller_pc.local_description()
                    && answer.sdp_type == rustrtc::SdpType::Pranswer
                {
                    answer.sdp_type = rustrtc::SdpType::Answer;
                    if let Err(error) = caller_pc.set_local_description(answer) {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            error = %error,
                            "Failed to promote caller local pranswer"
                        );
                    }
                }
            }

            return Ok(if self.media.early_media_sent {
                self.media.answer.clone()
            } else {
                None
            });
        };

        let sdp_changed =
            self.media.callee_answer_sdp.as_deref() != Some(callee_sdp_value.as_str());

        if self
            .caller_dialog
            .as_ref()
            .is_some_and(|d| d.state().is_confirmed())
            && self.media.answer.is_some()
            && !sdp_changed
            && !force_regenerate
        {
            return Ok(self.media.answer.clone());
        }

        if self.media.callee_answer_sdp.is_some() && sdp_changed {
            debug!(session_id = %self.id,
                session_id = %self.context.session_id,
                "Callee SDP changed after early media; updating the existing media transport"
            );
        }

        let callee_sdp = Some(callee_sdp_value);
        let caller_answer = if self.media_profile.path == MediaPathMode::Anchored {
            if let Some(peer) = self.media_leg(&LegId::from("callee")) {
                peer.apply_sdp(callee_sdp.as_deref().unwrap(), callee_sdp_type)
                    .await?;
            }
            self.ensure_caller_leg().await?;
            self.media.answer.clone()
        } else {
            callee_sdp.clone()
        };

        self.media.callee_answer_sdp = callee_sdp.clone();
        self.media.answer = caller_answer.clone();

        Ok(caller_answer)
    }

    /// Classify a peer's media transport from its SDP:
    /// - `WebRtc` when it carries ICE + DTLS (`a=ice-ufrag` + `a=fingerprint`),
    /// - `Srtp` for SDES-SRTP (`RTP/SAVP` profile or an `a=crypto` line) without DTLS,
    /// - `Rtp` otherwise (plain `RTP/AVP`).
    pub(crate) fn sdp_transport_mode(sdp: &str) -> rustrtc::TransportMode {
        if sdp.contains("a=ice-ufrag") && sdp.contains("a=fingerprint") {
            rustrtc::TransportMode::WebRtc
        } else if sdp.contains("RTP/SAVP") || sdp.contains("a=crypto") {
            rustrtc::TransportMode::Srtp
        } else {
            rustrtc::TransportMode::Rtp
        }
    }

    pub(crate) fn resolve_effective_codecs(&self) -> Vec<CodecType> {
        if !self.context.dialplan.allow_codecs.is_empty() {
            return self.context.dialplan.allow_codecs.clone();
        }

        if let Some(codecs) = self.match_destination_trunk_codecs() {
            return codecs;
        }

        if let Some(ref codecs) = self.server.proxy_config.load().audio_codecs {
            let parsed = parse_allowed_codecs(codecs);
            if !parsed.is_empty() {
                return parsed;
            }
        }

        vec![]
    }

    /// Try to find codecs by matching the callee URI host:port against trunk destinations.
    /// This covers both regular file-based trunks and DB-based (wholesale) trunks.
    pub(crate) fn match_destination_trunk_codecs(&self) -> Option<Vec<CodecType>> {
        let callee_uri = &self.context.dialplan.original.uri;
        let callee_host: String = callee_uri.host().to_string().to_lowercase();
        let callee_port: u16 = callee_uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);

        let trunks = self.server.data_context.trunks_snapshot();
        for (_name, trunk) in trunks.iter() {
            if trunk.codec.is_empty() {
                continue;
            }
            if let Some((trunk_host, trunk_port)) = trunk_host_port(&trunk.dest) {
                if trunk_host.to_lowercase() == callee_host && trunk_port == callee_port {
                    let parsed = parse_allowed_codecs(&trunk.codec);
                    if !parsed.is_empty() {
                        return Some(parsed);
                    }
                }
            }
        }
        None
    }

    pub(crate) async fn ensure_caller_answer_sdp(&mut self) -> Option<String> {
        if let Some(answer) = self.media.answer.clone() {
            return Some(answer);
        }
        if self.bypasses_local_media() {
            if let Some(answer) = self.media.callee_answer_sdp.clone() {
                self.media.answer = Some(answer.clone());
                return Some(answer);
            }
        }
        if let Err(error) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id, %error, "Failed to prepare caller media");
            return None;
        }
        self.media.answer.clone()
    }

    /// Generate hold (sendonly) / unhold (sendrecv) SDP for a side, reusing
    /// that side's last negotiated SDP (answer, else offer).
    pub(crate) fn generate_sdp_for_side(&self, side: &LegId, sendonly: bool) -> Result<String> {
        let direction = if sendonly { "sendonly" } else { "sendrecv" };
        self.sdp_for_side_with_direction(side, direction)
    }

    /// The side's last negotiated SDP (answer, else offer) rewritten to
    /// `direction`. Used when a relayed re-INVITE offer's transport style
    /// (RTP vs WebRTC) doesn't match what the target leg originally
    /// negotiated — the leg's own SDP is the only style it can process.
    pub(crate) fn sdp_for_side_with_direction(&self, side: &LegId, direction: &str) -> Result<String> {
        if let Some(peer) = self.legs.media_leg(side) {
            if let Some(sdp) = peer.pc().local_description() {
                return Ok(rustrtc::modify_sdp_direction(
                    &sdp.to_sdp_string(),
                    direction,
                ));
            }
        }
        let base_sdp = if side.0 == "callee" {
            self.media
                .callee_answer_sdp
                .as_deref()
                .or(self.media.callee_offer.as_deref())
        } else {
            self.media
                .answer
                .as_ref()
                .or(self.media.caller_offer.as_ref())
                .map(|s| s.as_str())
        }
        .ok_or_else(|| anyhow!("No SDP available for {} hold/unhold", side.0))?;
        Ok(rustrtc::modify_sdp_direction(base_sdp, direction))
    }

    /// Keep only the explicitly requested pair.
    /// Additional legs wait for explicit selection; only mixer APIs create a
    /// conference.
    pub(super) async fn update_media_path(&mut self) {
        if self.conference_bridge.conf_id.is_some() {
            return;
        }
        let requested = self.bridge.legs.clone();
        if requested.len() == 2
            && requested.iter().all(|id| {
                self.legs
                    .get(id)
                    .is_some_and(|leg| !matches!(leg.state, LegState::Ending | LegState::Ended))
            })
        {
            // Hold changes audio output, not the selected conversation partner.
            // A playback completion or unmute while held must neither reconnect
            // media nor discard the pair needed by the subsequent unhold.
            if requested.iter().any(|id| {
                self.legs
                    .get(id)
                    .is_some_and(|leg| leg.state == LegState::Hold)
            }) {
                return;
            }
            self.setup_bridge(requested[0].clone(), requested[1].clone())
                .await;
            return;
        }
        if self.bridge.active {
            self.clear_bridge().await;
        }
    }
}
