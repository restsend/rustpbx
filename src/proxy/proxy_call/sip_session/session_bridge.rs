
// ===================================================================
// Media-bridge lifecycle & control methods for SipSession
// (moved from sip_session/session.rs — the bridge-domain cluster:
// setup/teardown, bridged-leg queries, bridged RTP timeout arming).
// ===================================================================

use anyhow::Result;
use tracing::{debug, info};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::warn;

use crate::call::domain::{CallCommand, LegId};

impl crate::proxy::proxy_call::sip_session::session::SipSession {
    /// Cancel and drop any external (voip/realtime) WS bridge handle.
    /// Idempotent; safe from Drop (cancel is synchronous).
    pub(crate) fn teardown_external_bridge(&mut self, why: &str) {
        if let Some(handle) = self.external_bridge.take() {
            handle.cancel.cancel();
            debug!(session_id = %self.id, why, kind = ?handle.kind, "external bridge torn down");
        }
    }

    /// Attach one participant to a mixer and retain its bridge on the local
    /// leg. Other participants in this session keep their own bridge handles.
    pub(crate) async fn try_start_and_store_bridge(
        &mut self,
        conf_id: &str,
        leg: &LegId,
        label: &str,
    ) -> Result<()> {
        let prefix = format!("{}-", self.id);
        let local_leg = LegId::from(leg.as_str().strip_prefix(&prefix).unwrap_or(leg.as_str()));
        self.require_leg(&local_leg)?;
        let participant_leg = self.participant_leg(&local_leg);
        let current_room = self
            .server
            .conference_server
            .get_conference_id_for_leg(&participant_leg)
            .await;
        if let Some(room) = current_room.as_ref() {
            if room.0 == conf_id
                && self
                    .legs
                    .conference_bridge_handle(&local_leg)
                    .is_some_and(|handle| !handle.cancel_token.is_cancelled())
            {
                return Ok(());
            }
        }
        // Moving to another mixer must stop the old bridge and remove its
        // membership before registering this participant in the new room.
        drop(self.legs.remove_conference_bridge_handle(&local_leg));
        if let Some(room) = current_room {
            self.server
                .conference_server
                .remove_participant(&room, &participant_leg)
                .await?;
        }
        if self.conference_bridge.conf_id.is_none() {
            if let Some(mb) = self.bridge_mut() {
                mb.unbridge().await?;
            }
        }
        let handle = self
            .start_conference_media_bridge(conf_id, &participant_leg)
            .await
            .map_err(|error| {
                warn!(session_id = %self.id, %participant_leg, %error, "Failed to start {}", label);
                error
            })?;
        self.legs.set_conference_bridge_handle(local_leg, handle);
        self.conference_bridge.conf_id = Some(conf_id.to_string());
        info!(session_id = %self.id, leg_id = %participant_leg, "{} started", label);
        Ok(())
    }

    /// Arm the RTP inactivity timeout on both legs after a call is answered.
    /// If no RTP arrives on either leg within the configured window, send a
    /// Hangup command so the session tears down with `rtpTimeout` reason.
    pub(crate) fn arm_bridged_rtp_timeouts(
        mb: &crate::media::media_bridge::MediaBridge,
        rtp_timeout: Option<Duration>,
        cmd_tx: Option<mpsc::Sender<CallCommand>>,
        session_id: &str,
    ) {
        let Some(timeout) = rtp_timeout else {
            return;
        };
        for side in [
            crate::media::media_bridge::LegSide::A,
            crate::media::media_bridge::LegSide::B,
        ] {
            let Some(rx) = mb.arm_rtp_timeout(side, timeout) else {
                continue;
            };
            let cmd_tx = cmd_tx.clone();
            let sid = session_id.to_string();
            let rtp_side = match side {
                crate::media::media_bridge::LegSide::A => {
                    crate::call::domain::RtpTimeoutSide::Caller
                }
                crate::media::media_bridge::LegSide::B => {
                    crate::call::domain::RtpTimeoutSide::Callee
                }
            };
            crate::utils::spawn(async move {
                if rx.await.is_ok() {
                    warn!(session_id = %sid, leg_side = ?side, "RTP inactivity timeout fired");
                    if let Some(tx) = cmd_tx {
                        let _ = tx.try_send(CallCommand::Hangup(
                            crate::call::domain::HangupCommand::all(None, None)
                                .with_rtp_timeout_side(rtp_side),
                        ));
                    }
                }
            });
        }
    }

    /// Resolve the peer selected by the existing logical bridge. The fallback
    /// preserves direct caller/callee calls that predate explicit bridge state.
    pub(crate) fn bridged_peer_leg_id(&self, leg_id: &LegId) -> Option<LegId> {
        if self.conference.is_some() {
            return None;
        }
        if self.bridge.legs.len() == 2 && self.bridge.contains_leg(leg_id) {
            return self.bridge.legs.iter().find(|id| *id != leg_id).cloned();
        }
        if leg_id.as_str() == "caller" {
            let peer = self.resolve_transfer_leg();
            return self.is_caller_peer(&peer).then_some(peer);
        }
        self.is_caller_peer(leg_id).then(|| LegId::from("caller"))
    }

    pub(crate) async fn prepare_app_caller_media_bridge(&mut self) -> Option<String> {
        if let Err(e) = self.ensure_caller_leg().await {
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                error = %e,
                "Failed to ensure caller leg for app media bridge"
            );
            return None;
        }
        self.media.answer.clone()
    }

    /// Activate a logical bridge pair that `setup_bridge` deferred because a
    /// leg's media was not usable yet (answered, SDP/ICE still landing). The
    /// deferral keeps the pair "until usable media arrives" — but the arrival
    /// events used to never retry it: the negotiation handler was gated on
    /// `is_bridged()` (false for a deferred pair) and the leg-state update
    /// touches no media path, so a `CallCommand::Bridge` that raced ahead of
    /// the SDP stranded the call permanently unbridged (both legs emitting
    /// silence with RTP still flowing; a later hold fell into the
    /// signaling-only branch — no MOH, no log). The media-arrival sites call
    /// this to honor the promise. No-op unless a deferred pair exists.
    pub(crate) async fn retry_deferred_media_bridge(&mut self) {
        if self.bypasses_local_media() || self.bridge.legs.len() != 2 {
            return;
        }
        if self.bridge().is_some() {
            // Route bookkeeping exists — the normal media-path handling (and
            // the playback detach/restore machinery) owns this state.
            return;
        }
        let requested = self.bridge.legs.clone();
        self.update_media_path().await;
        if self.bridge().is_some_and(|bridge| bridge.is_bridged()) {
            info!(session_id = %self.id,
                leg_a = %requested[0], leg_b = %requested[1],
                "Deferred media bridge activated after usable media arrived"
            );
        }
    }

    pub(crate) async fn clear_bridge(&mut self) {
        let bridged: Vec<LegId> = self.bridge.legs.clone();
        for leg_id in &bridged {
            self.record_leg_event(
                &leg_id.0,
                crate::callrecord::LegTimelineEventType::Unbridged,
                None,
                None,
            );
        }
        self.bridge.clear();
        if self.media.bridge.is_some()
            && let Some(mb) = self.bridge_mut()
        {
            if let Err(e) = mb.clear_selection().await {
                warn!(session_id = %self.id, error = %e, "RWI unbridge failed");
            }
        }
    }
}
