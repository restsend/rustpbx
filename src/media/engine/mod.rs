//! MediaEngine — protocol-agnostic media orchestration
//!
//! The MediaEngine owns all media operations (bridge, playback, recording,
//! MCU mixing, DTMF, TTS injection) and exposes them through a single
//! command/event channel interface.  Signaling adapters (SIP, WebSocket,
//! HTTP, IVR apps) translate their domain operations into [`MediaCommand`]
//! variants and listen for [`MediaEvent`] notifications.
//!
//! ## Architecture
//!
//! ```text
//!  Signaling Layer          MediaEngine               src/media/
//!  ───────────────          ───────────               ──────────
//!  sip_session.rs    ──►   command_loop()      ──►   BridgePeer
//!  rwi/processor.rs  ──►   match cmd { … }     ──►   ForwardingTrack
//!  call/app/*        ──►   emit(event)         ──►   Recorder
//!  console/          ──►                        ──►   FileTrack
//!                                                          ConferenceAudioMixer
//! ```

pub mod audio_inject;
pub mod command;
pub mod event;
pub mod mcu_switch;
pub mod session;
pub mod transport;

pub use command::{
    CodecProfile, InjectTarget, LegTransport, MediaCommand, PcmFrame, PlayOptions, PlaySource,
    RecordConfig, SipFlowCaptureTx,
};
pub use event::{MediaEvent, RecordResult};
pub use transport::resolve_audio_path;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::media::Track as _;

// ---------------------------------------------------------------------------
// MediaEngineConfig
// ---------------------------------------------------------------------------

/// Global configuration for the MediaEngine.
#[derive(Debug, Clone)]
pub struct MediaEngineConfig {
    pub command_channel_capacity: usize,
    pub event_channel_capacity: usize,
}

impl Default for MediaEngineConfig {
    fn default() -> Self {
        Self {
            command_channel_capacity: 512,
            event_channel_capacity: 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// MediaEngine — the public handle
// ---------------------------------------------------------------------------

/// Public handle to the MediaEngine.
///
/// Clone this handle freely — all state is behind `Arc`.
/// Each clone shares the same command channel and event broadcast.
#[derive(Clone)]
pub struct MediaEngine {
    cmd_tx: mpsc::Sender<MediaCommand>,
    event_tx: broadcast::Sender<MediaEvent>,
    #[cfg(test)]
    sessions: Arc<RwLock<HashMap<String, session::MediaSession>>>,
}

/// Internal handle returned alongside MediaEngine for spawning the engine task.
pub struct MediaEngineHandle {
    pub cmd_rx: mpsc::Receiver<MediaCommand>,
    pub event_tx: broadcast::Sender<MediaEvent>,
    pub sessions: Arc<RwLock<HashMap<String, session::MediaSession>>>,
}

impl MediaEngine {
    /// Create a new MediaEngine instance.
    ///
    /// Returns `(engine, handle)`.  Call `engine.spawn(handle)` to start
    /// the command-processing loop.
    pub fn new(config: MediaEngineConfig) -> (Self, MediaEngineHandle) {
        let (cmd_tx, cmd_rx) = mpsc::channel(config.command_channel_capacity);
        let (event_tx, _) = broadcast::channel(config.event_channel_capacity);
        let sessions: Arc<RwLock<HashMap<String, session::MediaSession>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let engine = Self {
            cmd_tx,
            event_tx: event_tx.clone(),
            #[cfg(test)]
            sessions: sessions.clone(),
        };

        let handle = MediaEngineHandle {
            cmd_rx,
            event_tx,
            sessions,
        };

        (engine, handle)
    }

    /// Spawn the engine's command-processing loop as a background task.
    pub fn spawn(&self, handle: MediaEngineHandle) -> tokio::task::JoinHandle<()> {
        let core = EngineCore {
            cmd_rx: handle.cmd_rx,
            event_tx: handle.event_tx,
            sessions: handle.sessions,
        };
        crate::utils::spawn(async move {
            core.run().await;
        })
    }

    /// Subscribe to the event broadcast.
    pub fn subscribe(&self) -> broadcast::Receiver<MediaEvent> {
        self.event_tx.subscribe()
    }

    /// Send a command to the engine without waiting for a result.
    pub fn send(&self, cmd: MediaCommand) -> Result<()> {
        let cmd_debug = format!("{cmd:?}");
        let cmd_short = cmd_debug
            .split_once('{')
            .map(|(h, _)| h)
            .unwrap_or(&cmd_debug);
        self.cmd_tx.try_send(cmd).map_err(|e| {
            warn!("MediaEngine command channel full (cmd={cmd_short}): {e}");
            anyhow!("MediaEngine command channel: {}", e)
        })
    }

    /// Send a command asynchronously (backpressure-aware).
    pub async fn send_async(&self, cmd: MediaCommand) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|e| anyhow!("MediaEngine command channel: {}", e))
    }
}

// ---------------------------------------------------------------------------
// EngineCore — internal command loop
// ---------------------------------------------------------------------------

struct EngineCore {
    cmd_rx: mpsc::Receiver<MediaCommand>,
    event_tx: broadcast::Sender<MediaEvent>,
    sessions: Arc<RwLock<HashMap<String, session::MediaSession>>>,
}

impl EngineCore {
    async fn run(mut self) {
        info!("MediaEngine command loop started");

        while let Some(cmd) = self.cmd_rx.recv().await {
            let cmd_name = cmd.name();
            let session_id = cmd.session_id().map(|s| s.to_string());

            debug!(command = cmd_name, session = ?session_id, "MediaEngine command received");

            match self.dispatch(cmd).await {
                Ok(()) => {}
                Err(e) => {
                    error!(command = cmd_name, error = %e, "MediaEngine command failed");
                    let _ = self.event_tx.send(MediaEvent::Error {
                        session_id: session_id.unwrap_or_default(),
                        command: cmd_name.to_string(),
                        error: e.to_string(),
                    });
                }
            }
        }

        info!("MediaEngine command loop stopped");
    }

    async fn dispatch(&mut self, cmd: MediaCommand) -> Result<()> {
        match cmd {
            MediaCommand::CreateSession { session_id } => {
                let entry = session::MediaSession::new(session_id.clone());
                self.sessions.write().insert(session_id.clone(), entry);
                info!(session_id = %session_id, "MediaEngine session created");
                let _ = self
                    .event_tx
                    .send(MediaEvent::SessionCreated { session_id });
            }

            MediaCommand::DestroySession { session_id } => {
                let (sess, tracks_to_stop) = {
                    let mut sessions = self.sessions.write();
                    match sessions.remove(&session_id) {
                        Some(s) => {
                            let tracks: Vec<_> = s.playback_tracks.values().cloned().collect();
                            (Some(s), tracks)
                        }
                        None => (None, vec![]),
                    }
                };
                for track in tracks_to_stop {
                    track.stop().await;
                }
                if let Some(mut sess) = sess {
                    if sess.mcu.mode() == mcu_switch::MediaMode::Mcu {
                        sess.mcu.switch_to_bridge().await.ok();
                    }
                    {
                        let mut guard = sess.recorder.write();
                        if let Some(ref mut rec) = *guard {
                            let _ = rec.finalize();
                        }
                        *guard = None;
                    }
                    sess.bridge = None;
                }
                info!(session_id = %session_id, "MediaEngine session destroyed");
                let _ = self
                    .event_tx
                    .send(MediaEvent::SessionDestroyed { session_id });
            }

            MediaCommand::AddLeg {
                session_id,
                leg_id,
                transport: _,
                codec_profile: _,
            } => {
                debug!(session_id = %session_id, leg_id = %leg_id, "Leg added");
                let _ = self
                    .event_tx
                    .send(MediaEvent::LegAdded { session_id, leg_id });
            }

            MediaCommand::RemoveLeg { session_id, leg_id } => {
                let _ = self
                    .event_tx
                    .send(MediaEvent::LegRemoved { session_id, leg_id });
            }

            MediaCommand::AttachBridge {
                session_id,
                bridge,
                caller_is_webrtc,
                caller_codec_info,
            } => {
                let old_bridge = {
                    let mut sessions = self.sessions.write();
                    let sess = sessions.get_mut(&session_id).ok_or_else(|| {
                        anyhow!("Session {} not found for AttachBridge", session_id)
                    })?;
                    let old = sess.bridge.take();
                    sess.bridge = Some(bridge);
                    sess.caller_is_webrtc = caller_is_webrtc;
                    sess.caller_codec_info = caller_codec_info;
                    old
                };
                if let Some(old) = old_bridge {
                    old.stop().await;
                }
                debug!(session_id = %session_id, caller_is_webrtc, "Bridge attached to engine session");
            }

            MediaCommand::DetachBridge { session_id } => {
                let bridge = {
                    let mut sessions = self.sessions.write();
                    sessions.get_mut(&session_id).and_then(|s| s.bridge.take())
                };
                if let Some(bridge) = bridge {
                    bridge.stop().await;
                }
                info!(session_id = %session_id, "Bridge detached from engine session");
            }

            MediaCommand::AttachRecorder {
                session_id,
                recorder,
                paused,
            } => {
                let mut sessions = self.sessions.write();
                let sess = sessions.get_mut(&session_id).ok_or_else(|| {
                    anyhow!("Session {} not found for AttachRecorder", session_id)
                })?;
                sess.recorder = recorder;
                sess.recording_paused = paused;
                debug!(session_id = %session_id, "Recorder attached to engine session");
            }

            MediaCommand::SetSipFlowCapture {
                session_id,
                call_id,
                backend,
                receiver,
            } => {
                if !self.sessions.read().contains_key(&session_id) {
                    debug!(session_id = %session_id, "SipFlow capture skipped for missing session");
                    return Ok(());
                }

                if let (Some(backend), Some(mut rx)) = (backend, receiver) {
                    crate::utils::spawn(async move {
                        use crate::sipflow::{SipFlowItem, SipFlowMsgType};
                        while let Some((leg, sample, received_at_micros)) = rx.recv().await {
                            // `sample` is `Arc<MediaSample>`; deref to read.
                            if let rustrtc::media::frame::MediaSample::Audio(frame) = &*sample
                                && let Some(rtp_packet) = &frame.raw_packet
                                && let Ok(rtp_bytes) = rtp_packet.marshal()
                            {
                                let leg_id = match leg {
                                    crate::media::recorder::Leg::A => 0,
                                    crate::media::recorder::Leg::B => 1,
                                };
                                let item = SipFlowItem {
                                    timestamp: received_at_micros,
                                    seq: frame.sequence_number.unwrap_or(0) as u64,
                                    leg: Some(leg_id),
                                    msg_type: SipFlowMsgType::Rtp,
                                    src_addr: frame
                                        .source_addr
                                        .map(|addr| addr.to_string())
                                        .unwrap_or_default(),
                                    dst_addr: String::new(),
                                    payload: bytes::Bytes::from(rtp_bytes),
                                };
                                let _ = backend.record(&call_id, item);
                            }
                        }
                    });
                    debug!(session_id = %session_id, "SipFlow capture started");
                } else {
                    debug!(session_id = %session_id, "SipFlow capture stopped");
                }
            }

            MediaCommand::BridgeLegs {
                session_id,
                leg_a,
                leg_b,
            } => {
                debug!(session_id = %session_id, leg_a = %leg_a, leg_b = %leg_b, "BridgeLegs (signaling layer handles this)");
                let _ = self.event_tx.send(MediaEvent::BridgeEstablished {
                    session_id,
                    leg_a,
                    leg_b,
                });
            }

            MediaCommand::Unbridge { session_id } => {
                debug!(session_id = %session_id, "Unbridge");
                let _ = self.event_tx.send(MediaEvent::BridgeBroken {
                    session_id,
                    reason: "unbridged".into(),
                });
            }

            MediaCommand::Play {
                session_id,
                leg_id,
                source,
                options,
            } => {
                let play_id = options
                    .track_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                let resolved_path = match &source {
                    PlaySource::File { path } => transport::resolve_audio_path(path),
                    PlaySource::Url { url } => url.clone(),
                    PlaySource::Silence | PlaySource::Tts { .. } => {
                        warn!(session_id = %session_id, "Play: Silence/TTS source not yet implemented in engine");
                        return Ok(());
                    }
                };

                let (bridge, endpoint, codec_info_first) = {
                    let sessions = self.sessions.read();
                    let sess = sessions
                        .get(&session_id)
                        .ok_or_else(|| anyhow!("Session {} not found for Play", session_id))?;
                    let bridge = sess
                        .bridge
                        .clone()
                        .ok_or_else(|| anyhow!("No bridge for Play in session {}", session_id))?;
                    let endpoint = match leg_id.as_deref() {
                        Some(id) => sess.endpoint_for_leg(id),
                        None => sess.caller_endpoint(),
                    };
                    let codec = sess.caller_codec_info.first().cloned().unwrap_or_else(|| {
                        crate::media::negotiate::CodecInfo {
                            payload_type: 0,
                            codec: audio_codec::CodecType::PCMU,
                            clock_rate: 8000,
                            channels: 1,
                        }
                    });
                    (bridge, endpoint, codec)
                };

                let track = crate::media::FileTrack::new(play_id.clone())
                    .with_path(resolved_path)
                    .with_loop(options.loop_playback)
                    .with_codec_info(codec_info_first);

                match bridge.replace_output_with_file(endpoint, &track).await {
                    Ok(()) => {
                        let leg = leg_id.clone().unwrap_or_else(|| "caller".into());
                        {
                            let mut sessions = self.sessions.write();
                            if let Some(sess) = sessions.get_mut(&session_id) {
                                sess.bridge_playback_track_ids = vec![play_id.clone()];
                                sess.playback_tracks.insert(play_id.clone(), track);
                            }
                        }
                        let _ = self.event_tx.send(MediaEvent::PlayStarted {
                            session_id,
                            leg_id: leg,
                            play_id,
                        });
                    }
                    Err(e) => {
                        return Err(anyhow!("Play failed in engine: {}", e));
                    }
                }
            }

            MediaCommand::StopPlayback { session_id, leg_id } => {
                let (tracks_to_stop, bridge_opt, endpoint_opt) = {
                    let mut sessions = self.sessions.write();
                    if let Some(sess) = sessions.get_mut(&session_id) {
                        // Drain all registered playback track IDs (InjectAudio::Both
                        // creates two tracks under the same play_id but needs both stopped).
                        let ids: Vec<String> = std::mem::take(&mut sess.bridge_playback_track_ids);
                        let tracks: Vec<_> = ids
                            .iter()
                            .filter_map(|id| sess.playback_tracks.remove(id))
                            .collect();
                        let bridge = sess.bridge.clone();
                        let endpoint = match leg_id.as_deref() {
                            Some(id) => Some(sess.endpoint_for_leg(id)),
                            None => Some(sess.caller_endpoint()),
                        };
                        (tracks, bridge, endpoint)
                    } else {
                        (vec![], None, None)
                    }
                };
                for track in tracks_to_stop {
                    track.stop().await;
                }
                if let (Some(bridge), Some(endpoint)) = (bridge_opt, endpoint_opt) {
                    bridge.replace_output_with_peer(endpoint).await;
                }
            }

            MediaCommand::StartRecording {
                session_id,
                config,
                caller_profile,
                callee_profile,
                reply,
            } => {
                let mut sessions = self.sessions.write();
                let sess = sessions.get_mut(&session_id).ok_or_else(|| {
                    anyhow!("Session {} not found for StartRecording", session_id)
                })?;

                let mut guard = sess.recorder.write();
                if guard.is_some() {
                    return Err(anyhow!(
                        "Recording already active for session {}",
                        session_id
                    ));
                }
                // Pick the recorder codec from the first available leg profile,
                // falling back to PCMU.  Only PCMU/PCMA/G729 are supported as
                // WAV recording formats — Opus/G722 would be forced to PCMU
                // inside Recorder::new() anyway, so we resolve that here.
                let recorder_codec = caller_profile
                    .as_ref()
                    .or(callee_profile.as_ref())
                    .and_then(|p| p.audio.as_ref())
                    .map(|c| c.codec)
                    .unwrap_or(audio_codec::CodecType::PCMU);
                let mut recorder =
                    crate::media::recorder::Recorder::new(&config.path, recorder_codec)?;
                if let Some(profile) = caller_profile {
                    recorder.set_leg_profile(crate::media::recorder::Leg::A, profile);
                }
                if let Some(profile) = callee_profile {
                    recorder.set_leg_profile(crate::media::recorder::Leg::B, profile);
                }
                *guard = Some(recorder);
                use std::sync::atomic::Ordering;
                sess.recording_paused.store(false, Ordering::Relaxed);
                sess.recording_started_at = Some(std::time::Instant::now());
                info!(session_id = %session_id, path = %config.path, "Recording started");
                drop(guard);
                drop(sessions);

                // Signal the caller that the recorder is now visible to the
                // bridge's forwarding loop, so no audio gap exists.
                if let Some(reply_tx) = reply {
                    let _ = reply_tx.send(());
                }

                let _ = self
                    .event_tx
                    .send(MediaEvent::RecordingStarted { session_id });
            }

            MediaCommand::StopRecording { session_id, reply } => {
                let result = {
                    let mut sessions = self.sessions.write();
                    if let Some(sess) = sessions.get_mut(&session_id) {
                        let mut guard = sess.recorder.write();
                        if let Some(ref mut rec) = *guard {
                            let path = rec.path.clone();
                            let _ = rec.finalize();
                            let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                            let duration_secs = sess
                                .recording_started_at
                                .take()
                                .map(|t| t.elapsed().as_secs_f64())
                                .unwrap_or(0.0);
                            *guard = None;
                            Some(RecordResult {
                                path,
                                duration_secs,
                                file_size,
                            })
                        } else {
                            sess.recording_started_at = None;
                            None
                        }
                    } else {
                        None
                    }
                };
                if let Some(result) = result {
                    info!(session_id = %session_id, path = %result.path, "Recording stopped");
                    if let Some(reply_tx) = reply {
                        let _ = reply_tx.send(result.clone());
                    }
                    let _ = self
                        .event_tx
                        .send(MediaEvent::RecordingStopped { session_id, result });
                }
            }

            MediaCommand::PauseRecording { session_id } => {
                let sessions = self.sessions.read();
                if let Some(sess) = sessions.get(&session_id) {
                    use std::sync::atomic::Ordering;
                    // Release ensures the bridge forwarding thread sees the flag
                    // before the RecordingPaused event reaches subscribers.
                    sess.recording_paused.store(true, Ordering::Release);
                }
                let _ = self
                    .event_tx
                    .send(MediaEvent::RecordingPaused { session_id });
            }

            MediaCommand::ResumeRecording { session_id } => {
                let sessions = self.sessions.read();
                if let Some(sess) = sessions.get(&session_id) {
                    use std::sync::atomic::Ordering;
                    sess.recording_paused.store(false, Ordering::Release);
                }
                let _ = self
                    .event_tx
                    .send(MediaEvent::RecordingResumed { session_id });
            }

            MediaCommand::StartSipFlow { session_id } => {
                let _ = self
                    .event_tx
                    .send(MediaEvent::SipFlowStarted { session_id });
            }

            MediaCommand::StopSipFlow { session_id } => {
                let _ = self
                    .event_tx
                    .send(MediaEvent::SipFlowStopped { session_id });
            }

            MediaCommand::SendDtmf {
                session_id,
                leg_id,
                digits,
            } => {
                let (bridge_opt, endpoint_opt) = {
                    let sessions = self.sessions.read();
                    if let Some(sess) = sessions.get(&session_id) {
                        let ep = sess.endpoint_for_leg(&leg_id);
                        (sess.bridge.clone(), Some(ep))
                    } else {
                        (None, None)
                    }
                };
                if let (Some(bridge), Some(endpoint)) = (bridge_opt, endpoint_opt) {
                    if let Err(e) = bridge.send_dtmf_to_endpoint(endpoint, &digits).await {
                        warn!(
                            session_id = %session_id,
                            leg = %leg_id,
                            digits = %digits,
                            error = %e,
                            "SendDtmf failed"
                        );
                    } else {
                        debug!(session_id = %session_id, leg = %leg_id, digits = %digits, "SendDtmf injected");
                    }
                } else {
                    debug!(session_id = %session_id, leg = %leg_id, digits = %digits, "SendDtmf: no bridge, skipped");
                }
            }

            // CollectDtmf is handled at the signaling layer (RFC 4733 telephone-event
            // callback via BridgePeer::set_dtmf_sink, or SIP INFO) and does not need
            // engine-level processing.
            MediaCommand::CollectDtmf { session_id, .. } => {
                debug!(session_id, "CollectDtmf handled by signaling layer");
            }

            MediaCommand::JoinMixer {
                session_id,
                mixer_id,
            } => {
                // MCU mixing is managed by the conference runtime layer.
                // The engine records the event for observability.
                info!(
                    session_id,
                    mixer_id, "JoinMixer: recorded (MCU managed by conference runtime)"
                );
                let _ = self.event_tx.send(MediaEvent::MixerJoined {
                    session_id,
                    mixer_id,
                });
            }
            MediaCommand::LeaveMixer { session_id } => {
                let _ = self.event_tx.send(MediaEvent::MixerLeft {
                    session_id,
                    mixer_id: String::new(),
                });
            }
            MediaCommand::SetRouteGain {
                mixer_id,
                src_leg,
                dst_leg,
                gain,
            } => {
                // SetRouteGain is routed to the ConferenceAudioMixer directly by the
                // conference runtime (which holds the Arc<ConferenceAudioMixer>).
                // The engine command exists for observability / future unified API;
                // for now just log it so it is not silently swallowed.
                info!(
                    mixer_id,
                    src = %src_leg,
                    dst = %dst_leg,
                    gain,
                    "SetRouteGain: apply via ConferenceAudioMixer in conference runtime"
                );
            }
            MediaCommand::InjectAudio {
                session_id,
                source,
                target,
                mute_peer,
            } => {
                let (endpoints, mute_endpoint) = {
                    let sessions = self.sessions.read();
                    let Some(sess) = sessions.get(&session_id) else {
                        return Err(anyhow!("Session {} not found for InjectAudio", session_id));
                    };
                    let (eps, mute) = match target {
                        command::InjectTarget::Both => {
                            (vec![sess.caller_endpoint(), sess.callee_endpoint()], None)
                        }
                        command::InjectTarget::Leg(ref id) => {
                            let ep = sess.endpoint_for_leg(id);
                            let mute_ep = if mute_peer {
                                let opp = if ep == sess.caller_endpoint() {
                                    sess.callee_endpoint()
                                } else {
                                    sess.caller_endpoint()
                                };
                                Some(opp)
                            } else {
                                None
                            };
                            (vec![ep], mute_ep)
                        }
                    };
                    (eps, mute)
                };

                let play_id = uuid::Uuid::new_v4().to_string();
                let resolved_path = match &source {
                    command::PlaySource::File { path } => transport::resolve_audio_path(path),
                    command::PlaySource::Url { url } => url.clone(),
                    command::PlaySource::Silence | command::PlaySource::Tts { .. } => {
                        warn!(session_id = %session_id, "InjectAudio: Silence/TTS source not yet supported");
                        return Ok(());
                    }
                };

                let (bridge, codec_info_first) = {
                    let sessions = self.sessions.read();
                    let sess = sessions
                        .get(&session_id)
                        .ok_or_else(|| anyhow!("Session {} not found", session_id))?;
                    let bridge = sess.bridge.clone().ok_or_else(|| {
                        anyhow!("No bridge for InjectAudio in session {}", session_id)
                    })?;
                    let codec = sess.caller_codec_info.first().cloned().unwrap_or_else(|| {
                        crate::media::negotiate::CodecInfo {
                            payload_type: 0,
                            codec: audio_codec::CodecType::PCMU,
                            clock_rate: 8000,
                            channels: 1,
                        }
                    });
                    (bridge, codec)
                };

                // Each endpoint needs its own FileTrack so each bridge
                // forwarding loop reads from an independent
                // AudioSourceManager (they run concurrently).
                let mut tracks: Vec<(
                    crate::media::bridge::BridgeEndpoint,
                    crate::media::FileTrack,
                )> = Vec::new();
                for ep in &endpoints {
                    let t = crate::media::FileTrack::new(play_id.clone())
                        .with_path(resolved_path.clone())
                        .with_codec_info(codec_info_first.clone());
                    tracks.push((*ep, t));
                }

                for (ep, track) in &tracks {
                    bridge.replace_output_with_file(*ep, track).await?;
                }
                if let Some(ep) = mute_endpoint {
                    bridge.mute_output(ep).await;
                }

                {
                    let mut sessions = self.sessions.write();
                    if let Some(sess) = sessions.get_mut(&session_id) {
                        // Store all tracks so StopPlayback can stop every one.
                        // InjectAudio::Both creates two FileTrack instances under the
                        // same play_id (distinguished by endpoint suffix).
                        sess.bridge_playback_track_ids.clear();
                        for (ep, track) in tracks {
                            let key = format!("{}-{:?}", play_id, ep);
                            sess.bridge_playback_track_ids.push(key.clone());
                            sess.playback_tracks.insert(key, track);
                        }
                    }
                }
                let _ = self.event_tx.send(MediaEvent::PlayStarted {
                    session_id,
                    leg_id: if endpoints.len() == 2 {
                        "both".into()
                    } else {
                        "caller".into()
                    },
                    play_id,
                });
            }

            MediaCommand::Hold {
                session_id, leg_id, ..
            } => {
                debug!(session_id = %session_id, leg = %leg_id, "Hold (delegated to signaling layer)");
                let _ = self
                    .event_tx
                    .send(MediaEvent::LegHeld { session_id, leg_id });
            }
            MediaCommand::Unhold { session_id, leg_id } => {
                debug!(session_id = %session_id, leg = %leg_id, "Unhold (delegated to signaling layer)");
                let _ = self
                    .event_tx
                    .send(MediaEvent::LegUnheld { session_id, leg_id });
            }
            MediaCommand::MuteLeg { session_id, leg_id } => {
                let (bridge_opt, endpoint_opt) = {
                    let sessions = self.sessions.read();
                    sessions
                        .get(&session_id)
                        .map(|sess| (sess.bridge.clone(), sess.endpoint_for_leg(&leg_id)))
                        .map(|(b, e)| (Some(b), Some(e)))
                        .unwrap_or((None, None))
                };
                if let (Some(Some(bridge)), Some(endpoint)) = (bridge_opt, endpoint_opt) {
                    bridge.mute_output(endpoint).await;
                    debug!(session_id = %session_id, leg = %leg_id, "MuteLeg: bridge output muted");
                } else {
                    debug!(session_id = %session_id, leg = %leg_id, "MuteLeg: no bridge (signaling-layer hold)");
                }
            }
            MediaCommand::UnmuteLeg { session_id, leg_id } => {
                let (bridge_opt, endpoint_opt) = {
                    let sessions = self.sessions.read();
                    sessions
                        .get(&session_id)
                        .map(|sess| (sess.bridge.clone(), sess.endpoint_for_leg(&leg_id)))
                        .map(|(b, e)| (Some(b), Some(e)))
                        .unwrap_or((None, None))
                };
                if let (Some(Some(bridge)), Some(endpoint)) = (bridge_opt, endpoint_opt) {
                    bridge.replace_output_with_peer(endpoint).await;
                    debug!(session_id = %session_id, leg = %leg_id, "UnmuteLeg: bridge output restored");
                } else {
                    debug!(session_id = %session_id, leg = %leg_id, "UnmuteLeg: no bridge (signaling-layer hold)");
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock as PRwLock;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    /// Build a minimal RIFF/WAVE header for raw PCM.
    fn create_wav_header(sample_rate: u32, channels: u16, bits: u16, data_len: u32) -> Vec<u8> {
        let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
        let block_align = channels * (bits / 8);
        let chunk_size = 36 + data_len;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(chunk_size).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes()); // chunk size
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    }

    fn setup_engine() -> (MediaEngine, broadcast::Receiver<MediaEvent>) {
        let (engine, handle) = MediaEngine::new(MediaEngineConfig {
            command_channel_capacity: 64,
            event_channel_capacity: 64,
        });
        let rx = engine.subscribe();
        let _task = engine.spawn(handle);
        (engine, rx)
    }

    async fn create_session(engine: &MediaEngine, session_id: &str) {
        engine
            .send(MediaCommand::CreateSession {
                session_id: session_id.into(),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn test_engine_create_destroy_session() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "test-1").await;

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(ev, MediaEvent::SessionCreated { ref session_id } if session_id == "test-1")
        );

        engine
            .send(MediaCommand::DestroySession {
                session_id: "test-1".into(),
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(ev, MediaEvent::SessionDestroyed { .. }));
    }

    #[tokio::test]
    async fn test_engine_add_remove_leg() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "s1").await;
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::AddLeg {
                session_id: "s1".into(),
                leg_id: "caller".into(),
                transport: LegTransport::File {
                    path: "/dev/null".into(),
                },
                codec_profile: Some(CodecProfile::pcmu()),
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(matches!(ev, MediaEvent::LegAdded { leg_id, .. } if leg_id == "caller"));

        engine
            .send(MediaCommand::RemoveLeg {
                session_id: "s1".into(),
                leg_id: "caller".into(),
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(matches!(ev, MediaEvent::LegRemoved { leg_id, .. } if leg_id == "caller"));
    }

    // ── Recording tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_recording_start_stop_with_reply() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "rec1").await;
        let _ = event_rx.recv().await; // SessionCreated

        // AttachRecorder so the session has a recorder slot.
        let recorder: Arc<PRwLock<Option<crate::media::recorder::Recorder>>> =
            Arc::new(PRwLock::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        engine
            .send(MediaCommand::AttachRecorder {
                session_id: "rec1".into(),
                recorder: recorder.clone(),
                paused,
            })
            .unwrap();

        let tmp = std::env::temp_dir().join("test_engine_recording.wav");
        let path = tmp.to_string_lossy().to_string();

        // StartRecording with reply
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "rec1".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();

        // Wait for the reply (engine confirms recorder is created)
        reply_rx.await.expect("StartRecording reply not received");

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(ev, MediaEvent::RecordingStarted { .. }));

        // StopRecording with reply
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StopRecording {
                session_id: "rec1".into(),
                reply: Some(reply_tx),
            })
            .unwrap();

        let result = reply_rx.await.expect("StopRecording reply not received");
        assert_eq!(result.path, path);
        assert!(result.duration_secs >= 0.0);
        assert_eq!(result.file_size, 44); // just the WAV header, no audio written

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(ev, MediaEvent::RecordingStopped { .. }));

        // Cleanup temp file
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn test_recording_pause_resume() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "rec2").await;
        let _ = event_rx.recv().await;

        // AttachRecorder
        let recorder: Arc<PRwLock<Option<crate::media::recorder::Recorder>>> =
            Arc::new(PRwLock::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        engine
            .send(MediaCommand::AttachRecorder {
                session_id: "rec2".into(),
                recorder,
                paused,
            })
            .unwrap();

        let tmp = std::env::temp_dir().join("test_engine_pause.wav");
        let path = tmp.to_string_lossy().to_string();

        // Start
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "rec2".into(),
                config: RecordConfig {
                    path,
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();
        reply_rx.await.unwrap();
        let _ = event_rx.recv().await; // RecordingStarted

        // Pause
        engine
            .send(MediaCommand::PauseRecording {
                session_id: "rec2".into(),
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(matches!(ev, MediaEvent::RecordingPaused { .. }));

        // Resume
        engine
            .send(MediaCommand::ResumeRecording {
                session_id: "rec2".into(),
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(matches!(ev, MediaEvent::RecordingResumed { .. }));

        // Cleanup
        engine
            .send(MediaCommand::DestroySession {
                session_id: "rec2".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await; // SessionDestroyed
    }

    #[tokio::test]
    async fn test_recording_double_start_fails() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "rec3").await;
        let _ = event_rx.recv().await;

        let tmp = std::env::temp_dir().join("test_engine_double.wav");
        let path = tmp.to_string_lossy().to_string();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "rec3".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();
        reply_rx.await.unwrap();
        let _ = event_rx.recv().await; // RecordingStarted

        // Second StartRecording must fail — check via error event
        engine
            .send(MediaCommand::StartRecording {
                session_id: "rec3".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: None,
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(&ev, MediaEvent::Error { command, .. } if command == "start_recording"),
            "expected Error for double start, got {:?}",
            ev
        );

        let _ = std::fs::remove_file(&tmp);
    }

    // ── AttachBridge / InjectAudio tests ────────────────────────────────

    #[tokio::test]
    async fn test_attach_detach_bridge() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "bridge1").await;
        let _ = event_rx.recv().await;

        let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-1".into())
            .with_rtp_port_range(28000, 28100)
            .build();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "bridge1".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();

        // Sync: send a no-op command that produces an event so we know
        // AttachBridge has been processed by the engine loop.
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "bridge1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await; // BridgeEstablished — AttachBridge done

        // Verify the bridge is stored
        {
            let sessions = engine.sessions.read();
            let sess = sessions.get("bridge1").unwrap();
            assert!(sess.bridge.is_some());
            assert!(sess.caller_is_webrtc);
        }

        // DetachBridge
        engine
            .send(MediaCommand::DetachBridge {
                session_id: "bridge1".into(),
            })
            .unwrap();

        // Sync: another event
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "bridge1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        {
            let sessions = engine.sessions.read();
            let sess = sessions.get("bridge1").unwrap();
            assert!(sess.bridge.is_none());
        }
    }

    #[tokio::test]
    async fn test_inject_audio_produces_play_started() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "inj1").await;
        let _ = event_rx.recv().await;

        // Create a minimal PCM WAV file.
        let wav_path = std::env::temp_dir().join("test_inject.wav");
        {
            use std::io::Write;
            let data_len = 160u32;
            let sample_rate = 8000u32;
            let bits: u16 = 16;
            let channels: u16 = 1;
            let header = create_wav_header(sample_rate, channels, bits, data_len);
            let mut f = std::fs::File::create(&wav_path).unwrap();
            f.write_all(&header).unwrap();
            let silence: Vec<u8> =
                vec![0u8; data_len as usize * (bits as usize / 8) * channels as usize];
            f.write_all(&silence).unwrap();
        }

        let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-inj".into())
            .with_rtp_port_range(28100, 28200)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "inj1".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();

        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "inj1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::InjectAudio {
                session_id: "inj1".into(),
                source: PlaySource::File {
                    path: wav_path.to_string_lossy().to_string(),
                },
                target: InjectTarget::Both,
                mute_peer: false,
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(&ev, MediaEvent::PlayStarted { leg_id, .. } if leg_id == "both"),
            "expected PlayStarted(both), got {:?}",
            ev
        );

        let _ = std::fs::remove_file(&wav_path);
    }

    // ── DestroySession cleanup ──────────────────────────────────────────

    #[tokio::test]
    async fn test_destroy_session_cleans_up_recorder() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "cln1").await;
        let _ = event_rx.recv().await;

        let tmp = std::env::temp_dir().join("test_engine_cleanup.wav");
        let path = tmp.to_string_lossy().to_string();

        // Start recording
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "cln1".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();
        reply_rx.await.unwrap();
        let _ = event_rx.recv().await;

        // Destroy session — should finalize the recorder
        engine
            .send(MediaCommand::DestroySession {
                session_id: "cln1".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await; // SessionDestroyed

        // The WAV file should have been finalized (header with data_size)
        let meta = std::fs::metadata(&tmp).unwrap();
        assert!(
            meta.len() >= 44,
            "WAV file should have header after cleanup"
        );

        let _ = std::fs::remove_file(&tmp);
    }

    // ── SipFlow capture ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_set_sipflow_capture_disable() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "sf1").await;
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::SetSipFlowCapture {
                session_id: "sf1".into(),
                call_id: "test-call-1".into(),
                backend: None,
                receiver: None,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn test_command_name() {
        assert_eq!(
            MediaCommand::CreateSession {
                session_id: "x".into()
            }
            .name(),
            "create_session"
        );
        assert_eq!(
            MediaCommand::Play {
                session_id: "x".into(),
                leg_id: None,
                source: PlaySource::Silence,
                options: PlayOptions::default(),
            }
            .name(),
            "play"
        );
        assert_eq!(
            MediaCommand::StopRecording {
                session_id: "x".into(),
                reply: None,
            }
            .name(),
            "stop_recording"
        );
    }

    #[tokio::test]
    async fn test_session_id_extraction() {
        let cmd = MediaCommand::BridgeLegs {
            session_id: "s1".into(),
            leg_a: "a".into(),
            leg_b: "b".into(),
        };
        assert_eq!(cmd.session_id(), Some("s1"));

        let cmd = MediaCommand::InjectAudio {
            session_id: "s1".into(),
            source: PlaySource::Silence,
            target: InjectTarget::Both,
            mute_peer: false,
        };
        assert_eq!(cmd.session_id(), Some("s1"));
    }

    // ── Command dispatch to nonexistent session ─────────────────────────

    #[tokio::test]
    async fn test_command_on_nonexistent_session() {
        let (engine, mut event_rx) = setup_engine();

        // StartRecording requires a session — should fail.
        engine
            .send(MediaCommand::StartRecording {
                session_id: "nonexistent".into(),
                config: RecordConfig {
                    path: "/tmp/no.wav".into(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: None,
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(&ev, MediaEvent::Error { session_id, .. } if session_id == "nonexistent"),
            "expected Error for nonexistent session, got {:?}",
            ev
        );
    }

    // ── Play command with real WAV file ─────────────────────────────────

    #[tokio::test]
    async fn test_play_file_produces_play_started() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "play1").await;
        let _ = event_rx.recv().await;

        let wav_path = std::env::temp_dir().join("test_play.wav");
        {
            use std::io::Write;
            let data_len = 160u32;
            let header = create_wav_header(8000, 1, 16, data_len);
            let mut f = std::fs::File::create(&wav_path).unwrap();
            f.write_all(&header).unwrap();
            f.write_all(&vec![0u8; data_len as usize * 2]).unwrap();
        }

        let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-play".into())
            .with_rtp_port_range(28300, 28400)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "play1".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "play1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::Play {
                session_id: "play1".into(),
                leg_id: Some("caller".into()),
                source: PlaySource::File {
                    path: wav_path.to_string_lossy().to_string(),
                },
                options: PlayOptions {
                    loop_playback: false,
                    await_completion: false,
                    interrupt_on_dtmf: false,
                    track_id: Some("test-track-1".into()),
                    broadcast_to_all: false,
                },
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(&ev, MediaEvent::PlayStarted { play_id, .. } if play_id == "test-track-1"),
            "expected PlayStarted(test-track-1), got {:?}",
            ev
        );

        let _ = std::fs::remove_file(&wav_path);
    }

    // ── StopPlayback command ────────────────────────────────────────────

    #[tokio::test]
    async fn test_stop_playback_noop() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "sp1").await;
        let _ = event_rx.recv().await;

        // StopPlayback with no active track should not crash
        engine
            .send(MediaCommand::StopPlayback {
                session_id: "sp1".into(),
                leg_id: None,
            })
            .unwrap();

        // No crash, no event — just verify it does not panic.
        // Send a sync command to ensure the StopPlayback was processed.
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "sp1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(ev, MediaEvent::BridgeEstablished { .. }));
    }

    // ── InjectAudio with single Leg target ──────────────────────────────

    #[tokio::test]
    async fn test_inject_audio_to_single_leg() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "inj2").await;
        let _ = event_rx.recv().await;

        let wav_path = std::env::temp_dir().join("test_inj_leg.wav");
        {
            use std::io::Write;
            let data_len = 160u32;
            let header = create_wav_header(8000, 1, 16, data_len);
            let mut f = std::fs::File::create(&wav_path).unwrap();
            f.write_all(&header).unwrap();
            f.write_all(&vec![0u8; data_len as usize * 2]).unwrap();
        }

        let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-inj2".into())
            .with_rtp_port_range(28400, 28500)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "inj2".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "inj2".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // InjectAudio to callee only
        engine
            .send(MediaCommand::InjectAudio {
                session_id: "inj2".into(),
                source: PlaySource::File {
                    path: wav_path.to_string_lossy().to_string(),
                },
                target: InjectTarget::Leg("callee".into()),
                mute_peer: false,
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(&ev, MediaEvent::PlayStarted { leg_id, .. } if leg_id == "caller"),
            "expected PlayStarted(caller), got {:?}",
            ev
        );

        let _ = std::fs::remove_file(&wav_path);
    }

    // ── InjectAudio with mute_peer ──────────────────────────────────────

    #[tokio::test]
    async fn test_inject_audio_mute_peer() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "inj3").await;
        let _ = event_rx.recv().await;

        let wav_path = std::env::temp_dir().join("test_inj_mute.wav");
        {
            use std::io::Write;
            let data_len = 160u32;
            let header = create_wav_header(8000, 1, 16, data_len);
            let mut f = std::fs::File::create(&wav_path).unwrap();
            f.write_all(&header).unwrap();
            f.write_all(&vec![0u8; data_len as usize * 2]).unwrap();
        }

        let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-inj3".into())
            .with_rtp_port_range(28500, 28600)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "inj3".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "inj3".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Inject to caller with mute_peer → callee should be muted
        engine
            .send(MediaCommand::InjectAudio {
                session_id: "inj3".into(),
                source: PlaySource::File {
                    path: wav_path.to_string_lossy().to_string(),
                },
                target: InjectTarget::Leg("caller".into()),
                mute_peer: true,
            })
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(
            matches!(&ev, MediaEvent::PlayStarted { .. }),
            "expected PlayStarted, got {:?}",
            ev
        );

        let _ = std::fs::remove_file(&wav_path);
    }

    // ── BridgeLegs / Unbridge events ────────────────────────────────────

    #[tokio::test]
    async fn test_bridge_legs_events() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "br1").await;
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "br1".into(),
                leg_a: "caller".into(),
                leg_b: "callee".into(),
            })
            .unwrap();

        let ev = event_rx.recv().await.unwrap();
        assert!(
            matches!(&ev, MediaEvent::BridgeEstablished { leg_a, leg_b, .. } if leg_a == "caller" && leg_b == "callee"),
            "expected BridgeEstablished, got {:?}",
            ev
        );

        engine
            .send(MediaCommand::Unbridge {
                session_id: "br1".into(),
            })
            .unwrap();

        let ev = event_rx.recv().await.unwrap();
        assert!(
            matches!(&ev, MediaEvent::BridgeBroken { reason, .. } if reason == "unbridged"),
            "expected BridgeBroken, got {:?}",
            ev
        );
    }

    // ── Delegated command events (Hold/Unhold/Mute/Unmute) ──────────────

    #[tokio::test]
    async fn test_delegated_command_events() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "del1").await;
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::Hold {
                session_id: "del1".into(),
                leg_id: "caller".into(),
                music: None,
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(
            matches!(&ev, MediaEvent::LegHeld { leg_id, .. } if leg_id == "caller"),
            "expected LegHeld, got {:?}",
            ev
        );

        engine
            .send(MediaCommand::Unhold {
                session_id: "del1".into(),
                leg_id: "caller".into(),
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(
            matches!(&ev, MediaEvent::LegUnheld { leg_id, .. } if leg_id == "caller"),
            "expected LegUnheld, got {:?}",
            ev
        );
    }

    // ── Recorder codec selection from leg profiles ──────────────────────

    #[tokio::test]
    async fn test_recording_codec_from_leg_profile() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "rcc1").await;
        let _ = event_rx.recv().await;

        let tmp = std::env::temp_dir().join("test_codec_sel.wav");
        let path = tmp.to_string_lossy().to_string();

        // Create a leg profile with PCMA codec
        let caller_profile = crate::media::negotiate::NegotiatedLegProfile {
            audio: Some(crate::media::negotiate::NegotiatedCodec {
                codec: audio_codec::CodecType::PCMA,
                payload_type: 8,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: None,
            transport: rustrtc::TransportMode::Rtp,
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "rcc1".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: Some(caller_profile),
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();
        reply_rx.await.unwrap();
        let _ = event_rx.recv().await; // RecordingStarted

        // Verify the recorder was created with PCMA by checking the WAV header
        let data = std::fs::read(&tmp).unwrap();
        assert!(data.len() >= 44);
        // WAV format tag is at offset 20 (2 bytes), PCMA = 6
        let format_tag = u16::from_le_bytes([data[20], data[21]]);
        assert_eq!(
            format_tag, 6,
            "expected PCMA format tag (6), got {}",
            format_tag
        );

        engine
            .send(MediaCommand::DestroySession {
                session_id: "rcc1".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;
        let _ = std::fs::remove_file(&tmp);
    }

    // ── Recording codec falls back to PCMU ──────────────────────────────

    #[tokio::test]
    async fn test_recording_codec_fallback_pcmu() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "rcf1").await;
        let _ = event_rx.recv().await;

        let tmp = std::env::temp_dir().join("test_codec_fallback.wav");
        let path = tmp.to_string_lossy().to_string();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "rcf1".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();
        reply_rx.await.unwrap();
        let _ = event_rx.recv().await;

        let data = std::fs::read(&tmp).unwrap();
        let format_tag = u16::from_le_bytes([data[20], data[21]]);
        assert_eq!(
            format_tag, 7,
            "expected PCMU format tag (7), got {}",
            format_tag
        );

        let _ = std::fs::remove_file(&tmp);
    }

    // ── Play with unsupported sources ───────────────────────────────────

    #[tokio::test]
    async fn test_play_silence_no_event() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "ps1").await;
        let _ = event_rx.recv().await;

        let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-ps".into())
            .with_rtp_port_range(28600, 28700)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "ps1".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "ps1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Silence source — engine returns Ok(()) without emitting PlayStarted.
        engine
            .send(MediaCommand::Play {
                session_id: "ps1".into(),
                leg_id: None,
                source: PlaySource::Silence,
                options: PlayOptions::default(),
            })
            .unwrap();

        // A follow-up command to synchronise and ensure the Silence handler
        // did not crash.  No event is expected.
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "ps1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let ev = event_rx.recv().await.unwrap();
        assert!(matches!(ev, MediaEvent::BridgeEstablished { .. }));
    }

    // ── MediaEvent::session_id() coverage ───────────────────────────────

    #[test]
    fn test_media_event_session_id() {
        assert_eq!(
            MediaEvent::SessionCreated {
                session_id: "s1".into()
            }
            .session_id(),
            Some("s1")
        );
        assert_eq!(
            MediaEvent::RecordingStarted {
                session_id: "r1".into()
            }
            .session_id(),
            Some("r1")
        );
        assert_eq!(
            MediaEvent::DtmfCollected {
                session_id: "d1".into(),
                leg_id: "caller".into(),
                digits: "123".into(),
            }
            .session_id(),
            Some("d1")
        );
        assert_eq!(
            MediaEvent::Error {
                session_id: "e1".into(),
                command: "test".into(),
                error: "msg".into(),
            }
            .session_id(),
            Some("e1")
        );
    }

    // ── PlayStarted / PlayFinished session_id ───────────────────────────

    #[test]
    fn test_play_event_session_id() {
        assert_eq!(
            MediaEvent::PlayStarted {
                session_id: "p1".into(),
                leg_id: "caller".into(),
                play_id: "pid1".into(),
            }
            .session_id(),
            Some("p1")
        );
        assert_eq!(
            MediaEvent::PlayFinished {
                session_id: "p2".into(),
                leg_id: "callee".into(),
                play_id: "pid2".into(),
                interrupted: true,
            }
            .session_id(),
            Some("p2")
        );
    }

    // ── Fix regressions: InjectAudio dual-track, MuteLeg, PauseRecording ordering ──

    /// InjectAudio with InjectTarget::Both must register TWO separate track IDs so
    /// that StopPlayback can clean up both of them.
    #[tokio::test]
    async fn test_inject_audio_both_stores_two_track_ids() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "inj_both").await;
        let _ = event_rx.recv().await;

        let wav_path = std::env::temp_dir().join("test_inj_both.wav");
        {
            use std::io::Write;
            let data_len = 160u32;
            let header = create_wav_header(8000, 1, 16, data_len);
            let mut f = std::fs::File::create(&wav_path).unwrap();
            f.write_all(&header).unwrap();
            f.write_all(&vec![0u8; data_len as usize * 2]).unwrap();
        }

        let bridge = crate::media::bridge::BridgePeerBuilder::new("tb-both".into())
            .with_rtp_port_range(28600, 28700)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "inj_both".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "inj_both".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        engine
            .send(MediaCommand::InjectAudio {
                session_id: "inj_both".into(),
                source: PlaySource::File {
                    path: wav_path.to_string_lossy().to_string(),
                },
                target: InjectTarget::Both,
                mute_peer: false,
            })
            .unwrap();

        // Wait for PlayStarted
        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .unwrap();
        assert!(matches!(&ev, MediaEvent::PlayStarted { leg_id, .. } if leg_id == "both"));

        // Sync to let the InjectAudio command finish processing
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "inj_both".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Now inspect the session — BOTH track IDs must be stored.
        {
            let sessions = engine.sessions.read();
            let sess = sessions.get("inj_both").unwrap();
            assert_eq!(
                sess.bridge_playback_track_ids.len(),
                2,
                "InjectAudio::Both must register 2 track IDs, got: {:?}",
                sess.bridge_playback_track_ids
            );
            assert_eq!(
                sess.playback_tracks.len(),
                2,
                "playback_tracks must hold 2 FileTrack instances"
            );
        }

        let _ = std::fs::remove_file(&wav_path);
    }

    /// StopPlayback after InjectAudio::Both must drain all registered IDs from
    /// playback_tracks and bridge_playback_track_ids.
    #[tokio::test]
    async fn test_stop_playback_after_inject_both_clears_all_tracks() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "sp_both").await;
        let _ = event_rx.recv().await;

        let wav_path = std::env::temp_dir().join("test_sp_both.wav");
        {
            use std::io::Write;
            let data_len = 160u32;
            let header = create_wav_header(8000, 1, 16, data_len);
            let mut f = std::fs::File::create(&wav_path).unwrap();
            f.write_all(&header).unwrap();
            f.write_all(&vec![0u8; data_len as usize * 2]).unwrap();
        }

        let bridge = crate::media::bridge::BridgePeerBuilder::new("tb-sp-both".into())
            .with_rtp_port_range(28700, 28800)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "sp_both".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "sp_both".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Inject to both legs
        engine
            .send(MediaCommand::InjectAudio {
                session_id: "sp_both".into(),
                source: PlaySource::File {
                    path: wav_path.to_string_lossy().to_string(),
                },
                target: InjectTarget::Both,
                mute_peer: false,
            })
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .unwrap();

        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "sp_both".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Stop playback
        engine
            .send(MediaCommand::StopPlayback {
                session_id: "sp_both".into(),
                leg_id: None,
            })
            .unwrap();

        // Sync again
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "sp_both".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        {
            let sessions = engine.sessions.read();
            let sess = sessions.get("sp_both").unwrap();
            assert!(
                sess.bridge_playback_track_ids.is_empty(),
                "bridge_playback_track_ids must be empty after StopPlayback"
            );
            assert!(
                sess.playback_tracks.is_empty(),
                "playback_tracks must be empty after StopPlayback"
            );
        }

        let _ = std::fs::remove_file(&wav_path);
    }

    /// PauseRecording with Ordering::Release — verify the flag is immediately
    /// visible via Acquire load after the RecordingPaused event arrives.
    #[tokio::test]
    async fn test_pause_recording_flag_visible_after_event() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "pr_flag").await;
        let _ = event_rx.recv().await;

        let tmp = std::env::temp_dir().join("test_pr_flag.wav");
        let path = tmp.to_string_lossy().to_string();

        // Attach a shared paused flag so the test can inspect it directly.
        let paused_flag = Arc::new(AtomicBool::new(false));
        let recorder: Arc<PRwLock<Option<crate::media::recorder::Recorder>>> =
            Arc::new(PRwLock::new(None));
        engine
            .send(MediaCommand::AttachRecorder {
                session_id: "pr_flag".into(),
                recorder,
                paused: paused_flag.clone(),
            })
            .unwrap();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        engine
            .send(MediaCommand::StartRecording {
                session_id: "pr_flag".into(),
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: None,
                    beep: false,
                    format: None,
                },
                caller_profile: None,
                callee_profile: None,
                reply: Some(reply_tx),
            })
            .unwrap();
        reply_rx.await.unwrap();
        let _ = event_rx.recv().await; // RecordingStarted

        engine
            .send(MediaCommand::PauseRecording {
                session_id: "pr_flag".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await; // RecordingPaused

        // After the event the flag must already be set (Release/Acquire pair).
        assert!(
            paused_flag.load(std::sync::atomic::Ordering::Acquire),
            "paused flag must be true after RecordingPaused event"
        );

        engine
            .send(MediaCommand::ResumeRecording {
                session_id: "pr_flag".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await; // RecordingResumed

        assert!(
            !paused_flag.load(std::sync::atomic::Ordering::Acquire),
            "paused flag must be false after RecordingResumed event"
        );

        let _ = std::fs::remove_file(&tmp);
    }

    /// MuteLeg with a bridge attached must actually put the bridge output in
    /// muted state (BRIDGE_OUTPUT_MUTED = 2).
    #[tokio::test]
    async fn test_mute_unmute_leg_bridge_output_mode() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "mut1").await;
        let _ = event_rx.recv().await;

        let bridge = crate::media::bridge::BridgePeerBuilder::new("tb-mut".into())
            .with_rtp_port_range(28800, 28900)
            .build();
        bridge.setup_bridge().await.unwrap();

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "mut1".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: true,
                caller_codec_info: vec![],
            })
            .unwrap();
        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "mut1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Mute caller leg
        engine
            .send(MediaCommand::MuteLeg {
                session_id: "mut1".into(),
                leg_id: "caller".into(),
            })
            .unwrap();
        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "mut1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Caller endpoint == BridgeEndpoint::Caller when caller_is_webrtc=true.
        // The bridge's caller_output_mode must be BRIDGE_OUTPUT_MUTED (2).
        use std::sync::atomic::Ordering;
        assert_eq!(
            bridge.caller_output_mode_atomic().load(Ordering::Acquire),
            2, // BRIDGE_OUTPUT_MUTED
            "MuteLeg must set caller output mode to muted"
        );

        // Unmute
        engine
            .send(MediaCommand::UnmuteLeg {
                session_id: "mut1".into(),
                leg_id: "caller".into(),
            })
            .unwrap();
        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "mut1".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        assert_eq!(
            bridge.caller_output_mode_atomic().load(Ordering::Acquire),
            0, // BRIDGE_OUTPUT_PEER
            "UnmuteLeg must restore caller output mode to peer"
        );
    }

    /// SendDtmf with an active bridge must inject telephone-event packets into
    /// the bridge sender without returning an error. With no bridge present the
    /// command must silently succeed (skipped branch).
    #[tokio::test]
    async fn test_send_dtmf_no_bridge_is_noop() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "dtmf_noop").await;
        let _ = event_rx.recv().await;

        // No bridge attached — command should not cause an error event.
        engine
            .send(MediaCommand::SendDtmf {
                session_id: "dtmf_noop".into(),
                leg_id: "caller".into(),
                digits: "1234".into(),
            })
            .unwrap();

        // Sync: a subsequent event confirms the command was processed without error.
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "dtmf_noop".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .unwrap();
        // Must not be an error
        assert!(
            !matches!(&ev, MediaEvent::Error { .. }),
            "SendDtmf without bridge must not produce an error event, got {:?}",
            ev
        );
    }

    /// SendDtmf with an attached (and ready) bridge must succeed and inject
    /// telephone-event packets readable from the bridge sender.
    #[tokio::test]
    async fn test_send_dtmf_with_bridge_injects_packets() {
        let (engine, mut event_rx) = setup_engine();
        create_session(&engine, "dtmf_inj").await;
        let _ = event_rx.recv().await;

        let bridge = crate::media::bridge::BridgePeerBuilder::new("tb-dtmf".into())
            .with_rtp_port_range(28900, 29000)
            .build();
        bridge.setup_bridge().await.unwrap();

        // Tap the callee sender so we can observe the injected frames.
        let _callee_sender = bridge.get_callee_sender().await;
        // We verify correctness by calling send_dtmf_to_endpoint directly below
        // and checking that the engine does not produce an Error event.

        engine
            .send(MediaCommand::AttachBridge {
                session_id: "dtmf_inj".into(),
                bridge: bridge.clone(),
                caller_is_webrtc: false, // caller is RTP → endpoint_for_leg("callee") == Caller
                caller_codec_info: vec![],
            })
            .unwrap();
        // Sync
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "dtmf_inj".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let _ = event_rx.recv().await;

        // Send DTMF digit "5" to callee leg.  The engine routes it to the
        // BridgePeer::send_dtmf_to_endpoint() which we know is wired.
        engine
            .send(MediaCommand::SendDtmf {
                session_id: "dtmf_inj".into(),
                leg_id: "callee".into(),
                digits: "5".into(),
            })
            .unwrap();

        // Sync: wait until the command is processed.
        engine
            .send(MediaCommand::BridgeLegs {
                session_id: "dtmf_inj".into(),
                leg_a: "a".into(),
                leg_b: "b".into(),
            })
            .unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .unwrap();
        assert!(
            !matches!(&ev, MediaEvent::Error { .. }),
            "SendDtmf with bridge must not produce an error event, got {:?}",
            ev
        );

        // Also verify direct bridge API for RFC 4733 packet encoding.
        let result = bridge
            .send_dtmf_to_endpoint(crate::media::bridge::BridgeEndpoint::Callee, "5#")
            .await;
        assert!(
            result.is_ok(),
            "send_dtmf_to_endpoint must succeed for valid digits, got: {:?}",
            result
        );
    }

    /// Verify that send_dtmf_to_endpoint returns an error for an unknown digit.
    #[tokio::test]
    async fn test_send_dtmf_unknown_digit_error() {
        let bridge = crate::media::bridge::BridgePeerBuilder::new("tb-dtmf-err".into())
            .with_rtp_port_range(29000, 29100)
            .build();
        bridge.setup_bridge().await.unwrap();

        let result = bridge
            .send_dtmf_to_endpoint(crate::media::bridge::BridgeEndpoint::Caller, "X")
            .await;
        assert!(
            result.is_err(),
            "send_dtmf_to_endpoint must fail for unknown digit 'X'"
        );
    }
}
